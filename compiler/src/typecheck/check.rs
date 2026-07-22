//! The checker: a type for every expression.
//!
//! Bidirectional. `expected` flows down where a form can use it — a list's
//! elements, a lambda's parameters, an `if`'s arms — and types flow up everywhere
//! else. Where both meet, one rule decides: `actual <: expected`.
//!
//! Nothing here may invent a type when it does not know one. There is no `Erased`
//! to fall back to and no way to write one; when the checker cannot work something
//! out it emits a diagnostic and poisons that expression, so the cascade is one
//! error rather than twenty silent ones.

use super::dispatch::{self, DispatchError};
use super::env::{qualify, Env, TypeError, TypeErrorKind};
use super::narrow::{self, Projected};
use super::print::print;
use super::resolve::Scope;
use super::result::{DefKind, DefSite, TypecheckResult};
use super::types::TyId;
use crate::ast::{self, BinOp, Expr, ExprKind, UnOp};
use crate::lexer::Span;

/// A single module at the root path. Only tests and callers with nothing else to check
/// use this; a real compilation goes through `check_all` so the stdlib is checked into
/// the same result.
pub fn check_module(env: &mut Env, m: &ast::Module) -> (TypecheckResult, Vec<TypeError>) {
    check_all(env, &[(Vec::new(), m)])
}

/// Check every module of a compilation, accumulating into one `TypecheckResult`.
///
/// The stdlib is checked here too, at its own module path: its function bodies are real
/// Neon code that has to be lowered, and lowering reads types and call resolutions out of
/// this result. Ids are unique across modules (see `ast::number_exprs_from`), so one map
/// covers them all.
///
/// # The returned list is every error
///
/// Checking used to report through two channels: this return value, and `Env::errors`,
/// where resolving a type annotation raises. A caller reading only one of them silently
/// dropped the other's diagnostics — `let x: NoSuchType = 5` compiled, and the poison type
/// it produced reached codegen. There were 23 call sites and every one had to *remember*.
///
/// So this drains `Env::errors` into what it returns. Whatever the environment was already
/// holding when it arrived (declaration errors from `Env::build_with`) comes back too, so
/// the invariant needs no qualification: **read the return value and you have seen
/// everything**. `env.errors()` is empty afterwards, which also means a caller cannot
/// double-report by reading both.
///
/// Callers that gate on declarations — check `env.errors()` after `build_with` and refuse
/// to check bodies against signatures that did not resolve — are unaffected: they read that
/// list *before* this runs.
///
/// The result is sorted by span, so the two phases' diagnostics interleave in source order
/// rather than arriving as two batches, and deduplicated on (span, kind).
/// Every path a const's initialiser mentions, for the cycle walk.
///
/// Through `ast::visit` rather than a hand-rolled match: the traversal there is kept in
/// step with `ids::Numberer` arm for arm, so a new `ExprKind` cannot quietly become a
/// place a self-reference hides. Paths that are not consts are collected too and filtered
/// by the caller, which is the one that can resolve them.
fn const_refs(e: &Expr, out: &mut Vec<Vec<String>>) {
    struct Refs<'a>(&'a mut Vec<Vec<String>>);
    impl<'a> ast::visit::Visitor<'a> for Refs<'_> {
        fn expr(&mut self, e: &'a Expr) {
            if let ExprKind::Path(p) = &e.kind {
                self.0.push(p.clone());
            }
            ast::visit::walk_expr(self, e);
        }
    }
    ast::visit::Visitor::expr(&mut Refs(out), e);
}

pub fn check_all(
    env: &mut Env,
    modules: &[(Vec<String>, &ast::Module)],
) -> (TypecheckResult, Vec<TypeError>) {
    let mut c = Checker {
        env,
        result: TypecheckResult::default(),
        errors: vec![],
        locals: vec![],
        ret: None,
        throws: None,
        loop_breaks: vec![],
        throw_sinks: vec![],
        bounds: vec![],
        rigids: vec![],
        lambda_returns: vec![],
        lambda_throws: vec![],
        capture_floors: vec![],
        hidden_cache: std::collections::HashMap::new(),
        sealed_cache: std::collections::HashMap::new(),
    };
    for (path, m) in modules {
        c.decls(path, &m.decls);
    }
    // After the walk, because it reads the const table the environment finished building,
    // and *before* anything is lowered: lowering inlines a const's initialiser at each use
    // and would recurse forever on a cycle. This is the only thing standing between
    // `const A = B` / `const B = A` and a stack overflow in the compiler.
    c.const_cycles();
    // One mistake, one diagnostic. A generic call checks each argument twice -- once while
    // solving the callee's type parameters, then again under the solution, which is what
    // lets an expected type flow into a lambda argument -- so anything wrong *inside* an
    // argument was reported twice. Deduplicating the finished list is cheaper than
    // threading a "probing, stay quiet" mode through every expression form, and an
    // identical kind at an identical span is the same mistake by construction.
    //
    // The same pass folds in the environment's channel. `Env::error` is where resolving a
    // type annotation raises, and it fires *during* the walk above, so its diagnostics
    // belong to the same run and are deduplicated against the checker's on the same key.
    // Draining it is what makes this return value the only list a caller has to read.
    let mut errors = c.env.take_errors();
    errors.extend(std::mem::take(&mut c.errors));
    // Stable, so two diagnostics at one span keep the order they were raised in.
    errors.sort_by(|a, b| (a.span.start, a.span.end).cmp(&(b.span.start, b.span.end)));
    let mut seen = Vec::new();
    errors.retain(|e| {
        let key = (e.span.clone(), e.kind.clone());
        if seen.contains(&key) {
            return false;
        }
        seen.push(key);
        true
    });
    (c.result, errors)
}

/// What the enclosing function may fail with. A declared clause is a *type*, checked by
/// subtyping. `main`'s implicit channel is not a type at all — substituting ⊤ for it both
/// erased the error path and, because everything is a subtype of ⊤, silently switched off
/// the check that a thrown value is an error. It is a rule instead: whatever escapes must
/// implement `Error`, checked per throw site.
#[derive(Clone, Copy)]
enum Throws {
    Declared(TyId),
    ImplicitError,
    /// A lambda body. There is no syntax to declare a lambda's `throws` — like its
    /// return type, it is an output, derivable from the body — so whatever propagates
    /// out is collected (in `lambda_throws`) instead of checked against a clause.
    Infer,
}

struct Checker<'a> {
    env: &'a mut Env,
    result: TypecheckResult,
    errors: Vec<TypeError>,
    /// Innermost last. A name resolves to the nearest binding. Each carries the span
    /// it was bound at, so a diagnostic can point back at a name's origin, and what
    /// kind of binding it is, so an editor can too.
    locals: Vec<Vec<(String, TyId, Span, DefKind)>>,
    ret: Option<TyId>,
    throws: Option<Throws>,
    /// Break values of the enclosing loops, innermost last. A `loop` is the union
    /// of the values it breaks with.
    loop_breaks: Vec<Vec<TyId>>,
    /// Throws collected by the enclosing `try` bodies. A throwing call outside any
    /// `try` is a compile error; inside one, its error type lands here.
    throw_sinks: Vec<Vec<TyId>>,
    /// The current function's `where T: P` bounds, as (param name, protocol). A
    /// method call on a rigid `T` is only allowed to resolve through one of these.
    bounds: Vec<(String, super::env::ProtocolId)>,
    /// The current function's generic names, so a type written in its body -- `as T`,
    /// `is T`, `let x: T` -- resolves `T` as the rigid variable it introduced.
    rigids: Vec<String>,
    /// One frame per enclosing lambda, collecting the types its `return`s produce. A
    /// lambda declares no return type, so its type is the union of its tail and these.
    lambda_returns: Vec<Vec<TyId>>,
    /// One frame per enclosing lambda, collecting the error types that propagate out
    /// of it — a `throw` or a `try`-propagate in its body. A lambda declares no
    /// `throws` either; the union of these is the clause its arrow gets.
    lambda_throws: Vec<Vec<TyId>>,
    /// One entry per enclosing lambda: the `locals` depth where that lambda's own
    /// scope begins. A name found in a frame below the innermost floor was captured,
    /// and assigning to a capture is an error -- the closure holds a private copy.
    capture_floors: Vec<usize>,
    /// Per viewing module: the `#nominal` tags of opaque records that module may not
    /// see into, each with its owner. This is `Env::foreign_opaque_tags` memoised —
    /// the set is a function of the module alone, and the flow gate consults it on
    /// every checked expression.
    hidden_cache: std::collections::HashMap<
        Vec<String>,
        std::rc::Rc<std::collections::HashMap<super::types::NameId, Vec<String>>>,
    >,
    /// The `sealed` subset of the same, for the assertion ban and the `Ord` bar.
    sealed_cache: std::collections::HashMap<
        Vec<String>,
        std::rc::Rc<std::collections::HashMap<super::types::NameId, Vec<String>>>,
    >,
}

impl Checker<'_> {
    fn error(&mut self, span: Span, kind: TypeErrorKind) {
        self.errors.push(TypeError { span, kind });
    }

    /// A type as the user would write it, for a diagnostic. `&mut` because deciding
    /// whether to print a type or its complement interns the complement.
    fn show(&mut self, t: TyId) -> String {
        print(&mut self.env.solver.t, t)
    }

    /// The type an expression gets once it has already been reported on. It is a rigid
    /// variable under an unwritable name, not `never` — `never` is below everything and
    /// would check vacuously against whatever the expression flowed into, turning one
    /// mistake into a soundness hole rather than one diagnostic.
    fn poison(&mut self) -> TyId {
        self.env.error_ty()
    }

    /// Union of two branch types, absorbing poison. A branch that already produced
    /// a diagnostic must not make the whole `if`/`match` a `T | #error` that then
    /// fails to match its expected type -- one mistake, one error.
    fn union_branches(&mut self, a: TyId, b: TyId) -> TyId {
        if self.env.is_error(a) || self.env.is_error(b) {
            return self.poison();
        }
        self.env.solver.t.union(a, b)
    }

    /// `actual <: expected`, unless either is already poison — a checked
    /// expression that already produced a diagnostic must not produce a second.
    ///
    /// This is also where opacity is enforced against the *type-directed* routes. The
    /// syntactic doors (reading a field, building a literal, destructuring) are checked
    /// where they are written; but `Secret <: {code: i64}` is simply true — nominal
    /// satisfies structural by design — so a value could walk out through any position
    /// with a structural expected type: an argument, an annotation, a return, a record
    /// field, a list element, a lambda's parameter. All of those flows funnel through
    /// here, so here is where the question is asked a second time with the foreign
    /// opaque records' contents erased (`Types::seal`): if the subtyping only held by
    /// using those contents, the flow is the module reaching inside, and it is reported
    /// as exactly that — once, here, which is why this returns `true` after reporting
    /// rather than letting the caller add a generic mismatch on top.
    fn assignable(&mut self, module: &[String], span: &Span, actual: TyId, expected: TyId) -> bool {
        if self.env.is_error(actual) || self.env.is_error(expected) {
            return true;
        }
        if !self.env.solver.is_subtype(actual, expected) {
            return false;
        }
        if let Some((record, owner)) = self.opaque_flow(module, actual, expected) {
            self.report_opacity(
                module,
                span.clone(),
                &record,
                "it can be viewed as a structural type",
                owner,
            );
        }
        true
    }

    /// The tags `module` may not see into, memoised per module.
    fn hidden(
        &mut self,
        module: &[String],
    ) -> std::rc::Rc<std::collections::HashMap<super::types::NameId, Vec<String>>> {
        if let Some(h) = self.hidden_cache.get(module) {
            return h.clone();
        }
        let h = std::rc::Rc::new(self.env.foreign_opaque_tags(module));
        self.hidden_cache.insert(module.to_vec(), h.clone());
        h
    }

    /// The foreign `sealed` tags, memoised like `hidden`.
    fn hidden_sealed(
        &mut self,
        module: &[String],
    ) -> std::rc::Rc<std::collections::HashMap<super::types::NameId, Vec<String>>> {
        if let Some(h) = self.sealed_cache.get(module) {
            return h.clone();
        }
        let h = std::rc::Rc::new(self.env.foreign_sealed_tags(module));
        self.sealed_cache.insert(module.to_vec(), h.clone());
        h
    }

    /// The first foreign `sealed` record among `ty`'s nominal leaves, with its owner —
    /// the head-and-union-leaves scan the assertion ban and the `Ord` bar key on. The
    /// scan is deliberately leaf-level, not `mentions`-deep: a caller's own newtype
    /// *wrapping* a sealed type is the caller's type, and asserting it is legal — only
    /// the sealed type itself, named at the top or as a union member, is banned
    /// (docs/design/checked-casts.md, decisions and the newtype note).
    fn sealed_leaf(&mut self, module: &[String], ty: TyId) -> Option<(String, Vec<String>)> {
        let sealed = self.hidden_sealed(module);
        if sealed.is_empty() {
            return None;
        }
        for leaf in self.nominal_leaves(ty) {
            let id = self.env.solver.t.name(&leaf);
            if let Some(owner) = sealed.get(&id) {
                return Some((leaf, owner.clone()));
            }
        }
        None
    }

    /// Whether flowing a value of type `actual` into a position expecting `expected`
    /// would use the contents of an opaque record `module` may not see into. `None`
    /// when the flow is clean; otherwise the offending record's name and owner.
    ///
    /// The test: seal both sides — erase the hidden records' user fields — and re-ask
    /// the subtyping. Sealing both sides is what keeps naming the type legal (`Secret`
    /// flows into `Secret`, sealed into sealed, and the erasure cancels), while a
    /// structural view fails: the sealed value no longer promises the fields the view
    /// requires. Contravariance is covered by the same mechanism, because `seal`
    /// rewrites arrows' parameters too — a `({code: i64}) -> i64` lambda no longer
    /// passes as a `(Secret) -> i64`.
    fn opaque_flow(
        &mut self,
        module: &[String],
        actual: TyId,
        expected: TyId,
    ) -> Option<(String, Vec<String>)> {
        let hidden = self.hidden(module);
        if hidden.is_empty() {
            return None;
        }
        let tags: std::collections::HashSet<super::types::NameId> =
            hidden.keys().copied().collect();
        let t = &mut self.env.solver.t;
        if !t.mentions(actual, &tags) && !t.mentions(expected, &tags) {
            return None;
        }
        let sa = t.seal(actual, &tags);
        let se = t.seal(expected, &tags);
        if self.env.solver.is_subtype(sa, se) {
            return None;
        }
        Some(self.offending(&hidden, &tags, actual, expected))
    }

    /// The cast-shaped variant of `opaque_flow`. A cast may go up *or* down, so the
    /// sealed requirement is subsumption in either direction — or a newtype bridge that
    /// still holds once the representations are sealed, which is what keeps `s as Wrap`
    /// legal for a caller's own `newtype Wrap = vault::Secret` while rejecting the
    /// laundering `newtype W = {code: i64}; s as W`.
    ///
    /// Casting *to* a foreign opaque is its own case, checked first. A value of the
    /// target type must contain an opaque the module cannot construct, so the source
    /// has to *already name* it — otherwise the cast is fabricating one. This is what
    /// the sealed-subsumption test below cannot see: `any as Secret` passes it, because
    /// `sealed Secret <: any` holds (the branch that legitimately permits the widening
    /// `s as any`), so the same allowance green-lights the narrowing forge. `mentions`
    /// on the source is the honest line between recovering a `Secret` you hold — from a
    /// `Secret | str`, say — and conjuring one from a wildcard `any` or a bare shape.
    /// The cost is that recovering an opaque from an `any` that has erased its identity
    /// is refused: carry it as `Secret`, not as `any`.
    fn opaque_view(
        &mut self,
        module: &[String],
        from: TyId,
        to: TyId,
    ) -> Option<(String, Vec<String>, &'static str)> {
        let hidden = self.hidden(module);
        if hidden.is_empty() {
            return None;
        }
        let tags: std::collections::HashSet<super::types::NameId> =
            hidden.keys().copied().collect();
        // Forgery: the target produces an opaque the source cannot vouch for.
        //
        // Two ways a source vouches. It may *name* the opaque — `Secret | str as Secret`
        // is narrowing a value that provably holds one. Or it may be broad enough to
        // have legitimately held one: `any` admits every value, and widening an opaque
        // into `any` is a legal flow, so `(a: any) as List[i64]` is a *recovery* and is
        // the pinned erased-round-trip idiom (types/list_literal_erased_into_any_-
        // recovered.neon). A structural source is neither: outside the owner, the
        // assignable gate refuses `Secret -> {code: i64}`, so a `{code: i64}` value
        // provably never held a `Secret` and casting it to one is fabrication.
        //
        // `seal(to) <: from` is that second test exactly — sealed, so it asks whether the
        // source admits the opaque's *identity*, not its contents.
        //
        // What this deliberately does not do is make an unguarded `(a: any) as Secret`
        // safe. That cast is unchecked at run time — a general `as`-from-`any` hole, not
        // an opacity one — and the language's answer is `is` before `as`, where `is`
        // does compare nominal tags. Closing it belongs with the runtime tag check; see
        // docs/design/opacity.md.
        let sealed_to = self.env.solver.t.seal(to, &tags);
        let recovers = self.env.solver.is_subtype(sealed_to, from);
        for &tag in &tags {
            let single: std::collections::HashSet<_> = [tag].into();
            if self.env.solver.t.mentions(to, &single)
                && !self.env.solver.t.mentions(from, &single)
                && !recovers
            {
                let record = self.env.solver.t.name_str(tag).to_string();
                let owner = hidden.get(&tag).cloned().unwrap_or_default();
                return Some((record, owner, "it can be built"));
            }
        }
        {
            let t = &self.env.solver.t;
            if !t.mentions(from, &tags) && !t.mentions(to, &tags) {
                return None;
            }
        }
        let sf = self.env.solver.t.seal(from, &tags);
        let st = self.env.solver.t.seal(to, &tags);
        let ok = self.env.solver.is_subtype(sf, st)
            || self.env.solver.is_subtype(st, sf)
            || self.sealed_bridge(from, to, &tags)
            || self.sealed_bridge(to, from, &tags);
        if ok {
            return None;
        }
        let (record, owner) = self.offending(&hidden, &tags, from, to);
        Some((record, owner, "it can be cast to a structural view"))
    }

    /// `newtype_bridges`, re-asked with both the newtype's representation and the other
    /// side sealed: the wrap or unwrap is legal only if it does not depend on the hidden
    /// contents.
    fn sealed_bridge(
        &mut self,
        nt: TyId,
        other: TyId,
        tags: &std::collections::HashSet<super::types::NameId>,
    ) -> bool {
        let label = self.env.solver.t.name("#inner");
        // `Present` only: a real newtype declares `#inner` on its one atom. An open
        // structural type also *projects* the label — its `rest` admits anything, so the
        // projection comes back `Partial(any)` — and accepting that here made any open
        // struct a "newtype bridge" to anything, which is exactly the laundering this
        // check exists to refuse.
        let Projected::Present(inner) = narrow::project_field(&mut self.env.solver, nt, label)
        else {
            return false;
        };
        let si = self.env.solver.t.seal(inner, tags);
        let so = self.env.solver.t.seal(other, tags);
        self.env.solver.is_subtype(si, so) || self.env.solver.is_subtype(so, si)
    }

    /// The opacity gate for a dispatched call. Dispatch chooses impls by *overlap* with
    /// the receiver, not through `assignable`, so a receiver holding a foreign opaque
    /// record can select an impl written for a structural type — `impl Peek for
    /// {code: i64}` applies to a `Secret`, and the impl's body then reads the field
    /// legitimately from its own point of view. The chosen impl's target is a view the
    /// receiver flows into, so each chosen target gets the member-wise sealed question.
    fn dispatch_gate(
        &mut self,
        module: &[String],
        span: &Span,
        sel: &dispatch::Selection,
        args: &[TyId],
    ) {
        let Some(i) = sel.receiver_pos else { return };
        let Some(&recv) = args.get(i) else { return };
        let targets: Vec<TyId> = match &sel.resolution {
            dispatch::Resolution::Direct(id) => {
                self.env.impls()[id.0].target.into_iter().collect()
            }
            dispatch::Resolution::Switch(arms) => arms
                .iter()
                .filter_map(|&(_, id)| self.env.impls()[id.0].target)
                .collect(),
            dispatch::Resolution::Bound { .. } => vec![],
        };
        for target in targets {
            self.member_gate(
                module,
                span,
                recv,
                target,
                "it can satisfy an impl for a structural type",
            );
        }
    }

    /// The opacity gate for a discharged `where T: P` bound: inside the callee the
    /// bound resolves at run time to whichever impl of `P` covers the concrete type,
    /// so every covering impl's target is a view the value flows into.
    fn bound_gate(
        &mut self,
        module: &[String],
        span: &Span,
        concrete: TyId,
        pid: super::env::ProtocolId,
    ) {
        let targets: Vec<TyId> =
            self.env.impls_of(pid).filter_map(|(_, i)| i.target).collect();
        for target in targets {
            self.member_gate(
                module,
                span,
                concrete,
                target,
                "it can satisfy an impl for a structural type",
            );
        }
    }

    /// The member-wise sealed question: may each foreign opaque record among `value`'s
    /// leaves be seen through `view`?
    ///
    /// This is deliberately *not* `seal(value) <: seal(view)`. Where `value` is already
    /// an intersection — a dispatch arm, a bound's covered part, a narrowed binding —
    /// the whole-type question is vacuous: `Secret ∧ {code: i64} <: {code: i64}` holds
    /// sealed or not, because the structural conjunct carries the answer by itself. The
    /// non-vacuous question is about the opaque *member*: take each hidden-tagged atom
    /// in `value`'s leaves as the full type it denotes, and require that member, sealed,
    /// to still fit the sealed view. `Secret` fits `Secret`, `any`, and `Secret | X`;
    /// it does not fit `{code: i64}` once its contents are erased.
    fn member_gate(
        &mut self,
        module: &[String],
        span: &Span,
        value: TyId,
        view: TyId,
        what: &str,
    ) {
        let hidden = self.hidden(module);
        if hidden.is_empty() {
            return;
        }
        let tags: std::collections::HashSet<super::types::NameId> =
            hidden.keys().copied().collect();
        let members = self.hidden_members(value, &tags);
        for (tag, member) in members {
            let meet = self.env.solver.t.intersect(member, view);
            if self.env.solver.is_empty(meet) {
                continue;
            }
            let sm = self.env.solver.t.seal(member, &tags);
            let sv = self.env.solver.t.seal(view, &tags);
            if self.env.solver.is_subtype(sm, sv) {
                continue;
            }
            let record = self.env.solver.t.name_str(tag).to_string();
            let owner = hidden.get(&tag).cloned().unwrap_or_default();
            self.report_opacity(module, span.clone(), &record, what, owner);
        }
    }

    /// The hidden-tagged record atoms among `ty`'s positive leaves, each as the
    /// single-atom type it denotes. One entry per distinct atom.
    fn hidden_members(
        &mut self,
        ty: TyId,
        tags: &std::collections::HashSet<super::types::NameId>,
    ) -> Vec<(super::types::NameId, TyId)> {
        let mut atom_ids: Vec<u32> = Vec::new();
        {
            let t = &self.env.solver.t;
            let d = t.data(ty);
            for (pos, _) in t.rec_bdd.paths(d.records) {
                for i in pos {
                    let tag = t.rec_atoms[i as usize].get(t.nominal_label);
                    let atoms = t.atomset_of(t.data(tag).atoms);
                    if !atoms.neg
                        && atoms.names.len() == 1
                        && tags.contains(&atoms.names[0])
                        && !atom_ids.contains(&i)
                    {
                        atom_ids.push(i);
                    }
                }
            }
        }
        atom_ids
            .into_iter()
            .map(|i| {
                let a = self.env.solver.t.rec_atoms[i as usize].clone();
                let tag = self.env.solver.t.rec_atoms[i as usize].get(self.env.solver.t.nominal_label);
                let name = {
                    let t = &self.env.solver.t;
                    t.atomset_of(t.data(tag).atoms).names[0]
                };
                let member = self.env.solver.t.record(a);
                (name, member)
            })
            .collect()
    }

    /// Which hidden record to blame in a diagnostic: the first one the value's own type
    /// mentions, or failing that (the contravariant case, where the *expected* side
    /// names the record) the first the expected type mentions.
    fn offending(
        &mut self,
        hidden: &std::collections::HashMap<super::types::NameId, Vec<String>>,
        tags: &std::collections::HashSet<super::types::NameId>,
        actual: TyId,
        expected: TyId,
    ) -> (String, Vec<String>) {
        let t = &self.env.solver.t;
        // Sorted by name so the blamed record is deterministic when several qualify.
        let mut names: Vec<_> = tags.iter().copied().collect();
        names.sort_by_key(|&n| t.name_str(n).to_string());
        for n in names {
            let single: std::collections::HashSet<_> = [n].into();
            if t.mentions(actual, &single) || t.mentions(expected, &single) {
                let owner = hidden.get(&n).cloned().unwrap_or_default();
                return (t.name_str(n).to_string(), owner);
            }
        }
        (String::new(), vec![])
    }

    // ---- declarations ----

    /// Walk a module's declarations, checking every body there is one for. Signatures
    /// themselves were resolved earlier, in `Env`'s declaration phase; nothing here
    /// introduces a type, it only checks code against types already known. Nested `mod`s
    /// recurse with the module path extended, because that path is what decides name
    /// resolution and `internal`/`opaque` visibility for everything inside.
    ///
    /// A `test` block is checked as a function that may neither return nor throw: `ret`
    /// and `throws` are both `never`, so a stray `return x` or an uncaught throw inside
    /// one is a diagnostic rather than something with no enclosing signature to check
    /// against.
    fn decls(&mut self, module: &[String], decls: &[ast::Decl]) {
        for d in decls {
            match &d.kind {
                ast::DeclKind::Fn(f) => {
                    // `main`'s fixed signature is enforced in the declaration phase
                    // (`Env::fn_sig`), so an illegal clause is caught even when it
                    // would not resolve as a type.
                    self.fn_body(module, f, &[]);
                }
                ast::DeclKind::Impl(i) => {
                    for m in &i.methods {
                        self.fn_body(module, m, &i.generics);
                    }
                }
                ast::DeclKind::Protocol(p) => {
                    for m in &p.methods {
                        if m.body.is_some() {
                            self.fn_body(module, m, &[]);
                        }
                    }
                }
                ast::DeclKind::Const(c) => {
                    self.const_decl(module, c, &d.span);
                }
                ast::DeclKind::Mod(m) => {
                    let mut inner = module.to_vec();
                    inner.push(m.name.clone());
                    self.decls(&inner, &m.decls);
                }
                ast::DeclKind::TestBlock(t) => {
                    self.locals.push(vec![]);
                    let never = self.env.solver.t.never();
                    self.ret = Some(never);
                    self.throws = Some(Throws::Declared(never));
                    self.block(module, &t.body, None);
                    self.locals.pop();
                }
                _ => {}
            }
        }
    }

    /// Check one function body against its own signature.
    ///
    /// `outer` is the enclosing `impl`'s generics; they and the function's own are one
    /// rigid set, because a method may mention either and neither is instantiated here.
    /// Making them rigid rather than free is the point: inside the body `T` is opaque and
    /// disjoint from every concrete type, which is what makes `x is i64` on a `T` a
    /// reported mistake instead of a silently dead branch (see `narrow.rs`).
    ///
    /// The per-function state (`ret`, `throws`, `bounds`, `rigids`) is overwritten rather
    /// than saved and restored, which is safe only because declarations do not nest —
    /// a *lambda* is the case that does nest, and it saves and restores in `lambda`.
    ///
    /// Reject a `const` that is defined, directly or through others, in terms of itself.
    ///
    /// Depth-first over the reference graph, reporting the cycle at the declaration that
    /// closes it and printing the whole chain — `A -> B -> A` rather than a bare "cycle
    /// here", which leaves the author hunting for the other half.
    ///
    /// One report per cycle, not one per member: every const on a cycle would otherwise
    /// rediscover it and the same loop would be printed once for each name on it.
    fn const_cycles(&mut self) {
        let consts: Vec<(Vec<String>, String, ast::Expr, Span)> = self
            .env
            .consts()
            .iter()
            .map(|c| (c.module.clone(), c.name.clone(), c.value.clone(), c.span.clone()))
            .collect();
        let mut done: Vec<String> = Vec::new();
        for (module, name, _, span) in &consts {
            let key = qualify(module, name);
            if done.contains(&key) {
                continue;
            }
            let mut stack: Vec<String> = Vec::new();
            if let Some(chain) = self.walk_const(&consts, module, name, &mut stack, &mut done) {
                self.error(span.clone(), TypeErrorKind::ConstCycle(chain));
            }
        }
    }

    /// One depth-first step of `const_cycles`. Returns the cycle as a chain of names when
    /// this const reaches itself, closing the loop back to where it started.
    fn walk_const(
        &mut self,
        consts: &[(Vec<String>, String, ast::Expr, Span)],
        module: &[String],
        name: &str,
        stack: &mut Vec<String>,
        done: &mut Vec<String>,
    ) -> Option<Vec<String>> {
        let key = qualify(module, name);
        if let Some(at) = stack.iter().position(|k| *k == key) {
            // The chain from where the cycle opened, closed by repeating that name.
            let mut chain: Vec<String> = stack[at..].to_vec();
            chain.push(key);
            return Some(chain);
        }
        if done.contains(&key) {
            return None;
        }
        let (_, _, value, _) = consts.iter().find(|(m, n, _, _)| qualify(m, n) == key)?;
        stack.push(key.clone());
        let mut refs: Vec<Vec<String>> = Vec::new();
        const_refs(value, &mut refs);
        let mut found = None;
        for r in refs {
            // Resolved through the same rule a use site would use, so an imported or
            // qualified reference is followed rather than missed.
            let Some(target) = self.env.const_named(module, &r) else { continue };
            let (tm, tn) = (target.module.clone(), target.name.clone());
            if let Some(chain) = self.walk_const(consts, &tm, &tn, stack, done) {
                found = Some(chain);
                break;
            }
        }
        stack.pop();
        done.push(key);
        found
    }

    /// Check a top-level `const`: its initialiser against its annotation, and then that
    /// the initialiser is a thing the compiler can actually evaluate.
    ///
    /// The second half is the load-bearing one. Lowering inlines a const's initialiser at
    /// every use and leaves `ir::opt`'s folder to collapse it, so an initialiser the folder
    /// declines does not fail — it silently becomes *runtime* work, repeated at each use.
    /// `const_expr` therefore admits exactly what the folder folds, which is why it turns
    /// away float and string arithmetic that looks perfectly constant to a reader.
    fn const_decl(&mut self, module: &[String], c: &ast::ConstDecl, span: &Span) {
        // A missing annotation is already reported by `Env::resolve_bodies`, which is also
        // what decided there is no `ConstSig` to check against. Nothing to add here.
        let Some(sig) = self.env.const_named(module, std::slice::from_ref(&c.name)) else {
            return;
        };
        let want = sig.ty;
        self.locals.push(vec![]);
        let found = self.expr(module, &c.value, Some(want));
        self.locals.pop();
        self.assignable(module, &c.value.span, found, want);
        if let Err(what) = self.const_expr(module, &c.value) {
            self.error(span.clone(), TypeErrorKind::ConstNotConstant { name: c.name.clone(), what });
        }
    }

    /// Whether an expression is one `ir::opt`'s folder will reduce to a single constant,
    /// or `Err(what)` naming the first construct that is not.
    ///
    /// The admitted set is deliberately narrower than "looks constant":
    ///
    /// - A literal of any type is fine — it needs no folding at all.
    /// - Arithmetic, comparison and logic are fine **on `i64` and `bool` only**, because
    ///   `fold_int` and `fold_bool` are the whole of what the folder knows. `1.5 + 2.5` and
    ///   `"a" + "b"` are rejected: nothing folds them, so they would become a runtime add
    ///   and a runtime `neon_str_concat` at every use.
    /// - A path is fine when it names another `const`. The cycle that makes this dangerous
    ///   is caught separately, by `const_cycles`, before anything is lowered.
    /// - An interpolation is rejected even when its holes are constant: `"#{X}"` lowers to
    ///   `to_string` plus a concat, neither of which folds.
    fn const_expr(&mut self, module: &[String], e: &Expr) -> Result<(), String> {
        let foldable = |c: &mut Self, e: &Expr| -> bool {
            let Some(t) = c.result.ty(e.id) else { return false };
            let i = c.env.solver.t.i64();
            let b = c.env.solver.t.bool();
            t == i || t == b
        };
        match &e.kind {
            ExprKind::Int(_) | ExprKind::Float(_) | ExprKind::Bool(_) | ExprKind::Rune(_)
            | ExprKind::Atom(_) | ExprKind::Null => Ok(()),
            // A bare literal only. One `Text` part is `"abc"`; anything else has a hole.
            ExprKind::Str(parts) => match parts.as_slice() {
                [] | [ast::StrPart::Text(_)] => Ok(()),
                _ => Err("a string interpolation runs `to_string` and a concatenation, \
                          neither of which the compiler can fold"
                    .into()),
            },
            ExprKind::Path(p) => {
                if self.env.const_named(module, p).is_some() {
                    Ok(())
                } else {
                    Err(format!("`{}` is not a `const`", p.join("::")))
                }
            }
            ExprKind::Unary { rhs, .. } => {
                if !foldable(self, rhs) {
                    return Err("only `i64` and `bool` arithmetic folds".into());
                }
                self.const_expr(module, rhs)
            }
            ExprKind::Binary { lhs, rhs, .. } => {
                if !foldable(self, lhs) || !foldable(self, rhs) {
                    return Err("only `i64` and `bool` arithmetic folds; \
                                float and string operands do not"
                        .into());
                }
                self.const_expr(module, lhs)?;
                self.const_expr(module, rhs)
            }
            ExprKind::Call { .. } => Err("a call runs at run time".into()),
            ExprKind::List(_) | ExprKind::RecordLit { .. } | ExprKind::Tuple(_) => {
                Err("a list, record or tuple is built at run time and refcounted".into())
            }
            _ => Err("only literals, other `const`s, and integer and boolean arithmetic \
                      over those are constant"
                .into()),
        }
    }

    /// A declaration with no body is a bare signature — a protocol method, say — and has
    /// nothing to check.
    fn fn_body(&mut self, module: &[String], f: &ast::FnDecl, outer: &[String]) {
        let Some(body) = &f.body else { return };

        let mut scope = Scope::new(module);
        let mut generics: Vec<String> = outer.to_vec();
        generics.extend(f.generics.iter().cloned());
        scope = scope.with_rigid(self.env, &generics);
        self.rigids = generics;

        self.locals.push(vec![]);
        for p in &f.params {
            let t = self.env.resolve(&scope, &p.ty);
            self.bind(&p.name, t, p.span.clone(), DefKind::Param);
        }

        let ret = match &f.ret {
            Some(t) => self.env.resolve(&scope, t),
            None => self.env.solver.t.tuple(vec![]),
        };
        let throws = match &f.throws {
            Some(t) => Throws::Declared(self.env.resolve(&scope, t)),
            // `main`'s channel is a rule, not a type: whatever escapes must implement
            // `Error`, because `main` has to report it.
            None if module.is_empty() && f.name == "main" => Throws::ImplicitError,
            None => Throws::Declared(self.env.solver.t.never()),
        };
        self.ret = Some(ret);
        self.throws = Some(throws);
        self.bounds = f
            .wheres
            .iter()
            .filter_map(|w| match &w.bound.kind {
                ast::TypeSpecKind::Named { path, .. } => {
                    self.env.lookup_protocol(module, path).map(|p| (w.param.clone(), p))
                }
                _ => None,
            })
            .collect();

        // A fn returning `()` is a statement sequence -- its tail is whatever the last
        // statement happened to be, and nothing may be required of it. Anything else must
        // produce its return type as the tail.
        let unit = self.env.solver.t.tuple(vec![]);
        let want = if ret == unit { None } else { Some(ret) };
        self.block(module, body, want);
        self.locals.pop();
    }

    // ---- scopes ----

    /// Bind a name in the innermost frame. Shadowing is by push, not by replace: an
    /// existing binding of the same name stays in place and `lookup` finds the newer one
    /// because it scans each frame back to front. That is what lets a match arm rebind
    /// the scrutinee to its narrowed type without losing the outer binding when the arm
    /// ends.
    ///
    /// Silently does nothing when there is no frame, which only happens outside any
    /// declaration.
    ///
    /// `kind` is carried purely so a later jump-to-definition can say whether it landed
    /// on a parameter or a `let`. It takes no part in lookup: shadowing does not care
    /// what sort of binding it shadows.
    fn bind(&mut self, name: &str, t: TyId, span: Span, kind: DefKind) {
        if let Some(scope) = self.locals.last_mut() {
            scope.push((name.to_string(), t, span, kind));
        }
    }

    /// The nearest binding of `name`: innermost frame first, and within a frame the most
    /// recent push. Locals only — a function of this name is `path`'s business, and the
    /// order between the two is decided at each use site (`call` and `path` both try the
    /// local first, so a local shadows a fn).
    fn lookup(&self, name: &str) -> Option<TyId> {
        self.locals.iter().rev().flat_map(|s| s.iter().rev()).find(|(n, ..)| n == name).map(|(_, t, ..)| *t)
    }

    /// The index of the innermost `locals` frame that binds `name`, for deciding
    /// whether it lies below a lambda's capture floor.
    fn frame_of(&self, name: &str) -> Option<usize> {
        // Refinements are shadows of a binding, not bindings: a refinement of an outer
        // variable created inside a lambda must not make the variable look local to it.
        self.locals
            .iter()
            .enumerate()
            .rev()
            .find(|(_, s)| s.iter().any(|(n, _, _, k)| n == name && *k != DefKind::Refinement))
            .map(|(i, _)| i)
    }

    /// The span where `name` was bound, for a "captured here"-style secondary label.
    fn origin_of(&self, name: &str) -> Option<Span> {
        self.binding_of(name).map(|(s, _)| s)
    }

    /// Where `name` was bound and what sort of binding it is. The same innermost-first
    /// scan as `lookup`, so the three agree by construction about which binding a name
    /// means — a jump-to-definition that disagreed with the type shown on hover would be
    /// worse than no jump at all.
    fn binding_of(&self, name: &str) -> Option<(Span, DefKind)> {
        // Through refinements: a jump-to-def on a narrowed variable goes to the `let`,
        // not to the `is` that refined it.
        self.locals
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|(n, _, _, k)| n == name && *k != DefKind::Refinement)
            .map(|(_, _, s, k)| (s.clone(), *k))
    }

    /// The nearest non-refinement binding: the type `name` was declared at, which is
    /// what an assignment writes against. Reads use `lookup` and see refinements;
    /// writes see through them — and dissolve them, via `unrefine`.
    fn lookup_declared(&self, name: &str) -> Option<TyId> {
        self.locals
            .iter()
            .rev()
            .flat_map(|s| s.iter().rev())
            .find(|(n, _, _, k)| n == name && *k != DefKind::Refinement)
            .map(|(_, t, ..)| *t)
    }

    /// Dissolve every refinement of `name`: after an assignment the narrowed fact no
    /// longer holds, so the shadows are rewritten to the declared type rather than
    /// popped (they live in frames that are not ours to pop).
    fn unrefine(&mut self, name: &str, declared: TyId) {
        for frame in &mut self.locals {
            for entry in frame.iter_mut() {
                if entry.0 == name && entry.3 == DefKind::Refinement {
                    entry.1 = declared;
                }
            }
        }
    }

    // ---- blocks and statements ----

    /// A block's type is its tail's, or `()` when it has none. `expected` reaches only the
    /// tail — the statements above it are checked on their own terms, since none of them
    /// contributes to the value.
    ///
    /// The frame pushed here is what scopes `let`s to the block, and it is popped
    /// unconditionally: nothing in the checker aborts a block early, so there is no path
    /// that leaves a frame behind.
    fn block(&mut self, module: &[String], b: &ast::Block, expected: Option<TyId>) -> TyId {
        self.locals.push(vec![]);
        for s in &b.stmts {
            self.stmt(module, s);
        }
        let t = match &b.tail {
            Some(e) => self.expr(module, e, expected),
            None => self.env.solver.t.tuple(vec![]),
        };
        self.locals.pop();
        t
    }

    /// True when `nt` is a newtype whose representation meets `other` -- so a cast
    /// between the two only wraps or unwraps. A newtype carries its representation as
    /// a hidden `#inner` field, a label no source can write, so its presence marks a
    /// newtype and its type is the representation.
    fn newtype_bridges(&mut self, nt: TyId, other: TyId) -> bool {
        // Top is not a newtype: an open record projects *every* field as present, so
        // without this guard `any` bridged to everything — which made `any as str`
        // count as an infallible unwrap under the trichotomy.
        let top = self.env.solver.t.any();
        if self.env.solver.is_subtype(top, nt) {
            return false;
        }
        let label = self.env.solver.t.name("#inner");
        let Some(inner) = narrow::project_field(&mut self.env.solver, nt, label).ty() else {
            return false;
        };
        let meet = self.env.solver.t.intersect(inner, other);
        !self.env.solver.is_empty(meet)
    }

    /// A bare `if` (no `else`) has no value, so it cannot fill a value position whose
    /// expected type is unknown -- a binding without an annotation, or an argument to a
    /// protocol method, where there is nothing yet to reject it against. Where the
    /// expected type *is* known, `if_expr` rejects it against that type instead and this
    /// is not called; calling both would report the same mistake twice.
    fn reject_bare_if(&mut self, e: &Expr) {
        if let ExprKind::If { else_: None, .. } = &e.kind {
            self.error(e.span.clone(), TypeErrorKind::IfWithoutElse);
        }
    }

    /// A scope for resolving a type written in the current function's body. It
    /// carries the function's generics, so `as T` and `let x: T` see `T`.
    fn type_scope(&mut self, module: &[String]) -> Scope {
        let rigids = self.rigids.clone();
        Scope::new(module).with_rigid(self.env, &rigids)
    }

    /// A statement produces no type; everything it does is bind names and emit
    /// diagnostics. `ast::StmtKind::Error` is a parse error that already reported, and is
    /// deliberately silent here so one bad statement costs one message.
    fn stmt(&mut self, module: &[String], s: &ast::Stmt) {
        match &s.kind {
            ast::StmtKind::Let { pat, ty, value } => {
                let scope = self.type_scope(module);
                let want = ty.as_ref().map(|t| self.env.resolve(&scope, t));
                // A binding consumes a value. With an annotation, `if_expr` already
                // rejects a bare `if` against it; without one, there is no expected
                // type to catch it, so say so here.
                if want.is_none() {
                    self.reject_bare_if(value);
                }
                let t = self.expr(module, value, want);
                // The annotation is the binding's type when there is one: `let x:
                // i64|str = 1` binds the wider type, not `i64`. Record it against the
                // initialiser so lowering lays the binding out at the declared type too --
                // it sees only the initialiser, whose type is the narrow one.
                if let Some(w) = want {
                    self.result.set_declared(value.id, w);
                }
                self.bind_pattern(module, pat, want.unwrap_or(t));
            }
            ast::StmtKind::Assign { name, value } => {
                // Against the DECLARED type, not the narrowed one: `while cur is Node
                // { cur = cur.next }` writes a `Node | null` into a variable currently
                // refined to `Node`, and that is the loop working as intended. The
                // write also dissolves the refinement — after it, reads see the
                // declared type again.
                let Some(want) = self.lookup_declared(name) else {
                    self.error(s.span.clone(), TypeErrorKind::UnknownName(name.clone()));
                    self.expr(module, value, None);
                    return;
                };
                // A capture is immutable inside the closure: assigning to it would
                // write to the closure's private copy, invisible to everyone else.
                if let (Some(&floor), Some(frame)) = (self.capture_floors.last(), self.frame_of(name))
                {
                    if frame < floor {
                        let origin = self.origin_of(name);
                        self.error(
                            s.span.clone(),
                            TypeErrorKind::RebindCapture { name: name.clone(), origin },
                        );
                    }
                }
                // The RHS still sees the refined binding — `cur = cur.next` reads a
                // `Node`'s field — and only after it is checked does the refinement
                // dissolve: the write invalidates the narrowed fact.
                self.expr(module, value, Some(want));
                self.unrefine(name, want);
            }
            ast::StmtKind::Expr(e) => {
                self.expr(module, e, None);
            }
            ast::StmtKind::Error => {}
        }
    }

    /// Bind whatever names a pattern introduces, given the type the subject already
    /// narrowed to. This is binding only — the *test* a pattern performs is `arm_test`,
    /// and by the time this runs the caller has already intersected the subject with it.
    /// So `Is` and `Literal` bind nothing: they constrain, they do not name.
    ///
    /// Sub-patterns read through `project_field`/`project_elem` rather than the type
    /// being deconstructed by hand, which is what makes destructuring a union work: the
    /// projection takes the field over every variant that has it, and reports `Absent`
    /// (never `never`) where none does.
    fn bind_pattern(&mut self, module: &[String], p: &ast::Pattern, t: TyId) {
        match &p.kind {
            ast::PatternKind::Bind(n) => self.bind(n, t, p.span.clone(), DefKind::Local),
            ast::PatternKind::Wildcard => {}
            ast::PatternKind::Tuple(ps) => {
                for (i, sub) in ps.iter().enumerate() {
                    let e = narrow::project_elem(&mut self.env.solver, t, i);
                    let et = self.projected(sub.span.clone(), e, &i.to_string(), t);
                    self.bind_pattern(module, sub, et);
                }
            }
            ast::PatternKind::Record { fields, path, .. } => {
                // Destructuring is a field read that looks like a binding. The name comes
                // from the pattern's own path when it has one, and otherwise from the type
                // being matched.
                if let Some(q) = path {
                    if self.result.tested(p.id).is_none() {
                        self.record_tested_path(module, p, q);
                    }
                }
                match path {
                    Some(q) => self.check_opaque_path(
                        module, p.span.clone(), q, "it can be destructured"),
                    // Every nominal leaf, not just the single-atom case: destructuring
                    // a union or a narrowed intersection projects fields across all of
                    // its record leaves, exactly like a field read.
                    None => for n in self.nominal_leaves(t) {
                        self.check_opaque_name(
                            module, p.span.clone(), &n, "it can be destructured")
                    },
                }
                for f in fields {
                    let label = self.env.solver.t.name(&f.name);
                    let pj = narrow::project_field(&mut self.env.solver, t, label);
                    let ft = self.projected(p.span.clone(), pj, &f.name, t);
                    match &f.pat {
                        Some(sub) => self.bind_pattern(module, sub, ft),
                        None => self.bind(&f.name, ft, f.span.clone(), DefKind::Local),
                    }
                }
            }
            // Binds nothing, but it does *test*, and the test needs a resolved type — the
            // same reason `ExprKind::Is` records one. This runs for nested patterns too
            // (`Outer { inner: is List[str] }`), which `arm_test` never sees.
            ast::PatternKind::Is(spec) => {
                // Guarded: `arm_test` already resolved (and already reported) a top-level
                // arm pattern, and `resolve` is not idempotent in its diagnostics — a
                // second call on `case is Nope` would report the unknown name twice.
                if self.result.tested(p.id).is_none() {
                    let scope = self.type_scope(module);
                    let tested = self.env.resolve(&scope, spec);
                    // Same rule as `ExprKind::Is`: a nested structural test on an
                    // opaque-holding field is introspection.
                    self.member_gate(
                        module,
                        &p.span,
                        t,
                        tested,
                        "it can be tested against a structural view",
                    );
                    self.result.set_tested(p.id, tested);
                }
            }
            ast::PatternKind::Literal(_) | ast::PatternKind::Error => {}
        }
    }

    /// Record the type a named record pattern tests for, keyed on the pattern.
    fn record_tested_path(&mut self, module: &[String], p: &ast::Pattern, path: &[String]) {
        let scope = self.type_scope(module);
        let spec = ast::TypeSpec {
            kind: ast::TypeSpecKind::Named { path: path.to_vec(), args: vec![] },
            span: p.span.clone(),
        };
        let tested = self.env.resolve(&scope, &spec);
        self.result.set_tested(p.id, tested);
    }

    /// A projection's type, or a diagnostic. `Absent` carries no type on purpose:
    /// `never` would check vacuously against whatever the field went on to be used
    /// as, which is the trap this whole design keeps walking into.
    fn projected(&mut self, span: Span, p: Projected, label: &str, base: TyId) -> TyId {
        match p {
            Projected::Present(t) => t,
            // decisions.md has a missing field satisfy a nullable one, so an optional
            // field reads as `T | null` rather than as an error here.
            Projected::Partial(t) => {
                let null = self.env.solver.t.null();
                self.env.solver.t.union(t, null)
            }
            Projected::Absent => {
                if !self.env.is_error(base) {
                    let on = self.show(base);
                    self.error(span, TypeErrorKind::NoField { field: label.to_string(), on });
                }
                self.poison()
            }
        }
    }

    // ---- expressions ----

    /// The checking half of the bidirectional pair, and the only place `actual <:
    /// expected` is enforced. Everything that wants a type checked goes through here
    /// rather than calling `infer` directly, so no form can quietly skip the subtyping
    /// rule; `check_record_fields` is the one exception, and it calls `infer` precisely
    /// because it wants to report the mismatch as a *field* rather than as a bare type
    /// pair.
    ///
    /// Because every expression is reached through here, it is also where the type is
    /// recorded against the expression id for lowering to read back. `call` additionally
    /// records the *callee's* type, which is not an expression `expr` ever visits when
    /// the callee is a path.
    fn expr(&mut self, module: &[String], e: &Expr, expected: Option<TyId>) -> TyId {
        let t = self.infer(module, e, expected);
        if let Some(want) = expected {
            if !self.assignable(module, &e.span, t, want) {
                let (found, expect) = (self.show(t), self.show(want));
                self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: expect, found });
            } else if let (Some(a), Some(w)) = (
                self.env.solver.t.as_arrow(t),
                self.env.solver.t.as_arrow(want),
            ) {
                // Subtyping admits a function that throws less where one that throws
                // more is expected — but the `throws` clause is part of the calling
                // convention (a throwing closure returns a tagged result), and the
                // backend has no adapter between the two conventions: `coerce` passes a
                // `neon_closure` through unchanged, so the mismatch would compile clean
                // and read garbage. Until an adapter exists, require the clauses to
                // agree. Lambdas adopt the expected clause at creation, so this bites
                // only a previously-bound value flowing into a more-throwing slot.
                let same = self.env.solver.is_subtype(a.throws, w.throws)
                    && self.env.solver.is_subtype(w.throws, a.throws);
                if !same && !self.env.is_error(a.throws) && !self.env.is_error(w.throws) {
                    let (found, expected) = (self.show(a.throws), self.show(w.throws));
                    self.error(
                        e.span.clone(),
                        TypeErrorKind::ArrowThrowsMismatch { expected, found },
                    );
                }
            }
        }
        self.result.set_ty(e.id, t);
        t
    }

    /// The synthesis half: what an expression's type is on its own terms.
    ///
    /// `expected` is passed in rather than only consulted afterwards because a handful of
    /// forms genuinely need it to have a type at all — a lambda's unannotated parameters,
    /// a list literal's element type, an empty list, a generic call's type arguments.
    /// Everywhere else it is ignored here and the check happens in `expr`, which is why
    /// this is not "inference" in the unifying sense: nothing is solved from a later use.
    ///
    /// Every arm returns a type. Where one cannot be worked out, the arm reports and
    /// returns `poison`; there is no fallback type and no way to write one.
    fn infer(&mut self, module: &[String], e: &Expr, expected: Option<TyId>) -> TyId {
        match &e.kind {
            ExprKind::Int(_) => self.env.solver.t.i64(),
            ExprKind::Float(_) => self.env.solver.t.f64(),
            ExprKind::Bool(_) => self.env.solver.t.bool(),
            ExprKind::Null => self.env.solver.t.null(),
            ExprKind::Rune(_) => self.env.solver.t.i64(),
            ExprKind::Atom(a) => {
                let n = self.env.solver.t.name(a);
                self.env.solver.t.atom(n)
            }
            ExprKind::Str(parts) => {
                // `#{x}` desugars to `to_string(x)`, so an interpolated value must be
                // Display. Dispatching here is what enforces that, and records the
                // resolution for codegen.
                for p in parts {
                    if let ast::StrPart::Interp(inner) = p {
                        let t = self.expr(module, inner, None);
                        if !self.env.is_error(t) {
                            match dispatch::resolve(self.env, "to_string", None, &[t], None) {
                                Ok(sel) => {
                                    self.dispatch_gate(module, &inner.span, &sel, &[t]);
                                    self.result.set_call(inner.id, sel.resolution)
                                }
                                Err(err) => self.dispatch_error(inner.span.clone(), err),
                            }
                        }
                    }
                }
                self.env.solver.t.str()
            }

            ExprKind::Path(p) => self.path(module, e, p),

            ExprKind::Unary { op, rhs } => match op {
                UnOp::Neg | UnOp::Bnot => self.expr(module, rhs, None),
                // `not` is the one unary operator whose operand type is pinned by the
                // operator itself. Reading the operand with no expected type and then
                // returning `bool` regardless meant `!5` typechecked and produced a bool
                // out of an integer — a plausible value where a diagnostic belonged.
                UnOp::Not => {
                    let b = self.env.solver.t.bool();
                    self.expr(module, rhs, Some(b));
                    b
                }
            },

            ExprKind::Binary { op, lhs, rhs } => self.binary(module, e, *op, lhs, rhs, expected),

            ExprKind::Tuple(v) => {
                let ts: Vec<TyId> = v.iter().map(|x| self.expr(module, x, None)).collect();
                self.env.solver.t.tuple(ts)
            }

            ExprKind::List(elems) => {
                // Push the expected element type down. Without this a nested literal
                // infers its own type from its own elements — `[:ok, [:ok, :ok]]` against
                // `mu type A = :ok | List[A]` made the inner list a `List[:ok]` with
                // 8-byte slots where the outer expected 16-byte `A` slots, and the
                // coercion that could not bridge them quietly zeroed the element.
                let want_elem = expected.and_then(|t| self.element_type(t));
                let mut elem_tys = Vec::new();
                for el in elems {
                    match el {
                        ast::Elem::Value(x) => elem_tys.push(self.expr(module, x, want_elem)),
                        ast::Elem::Spread(x) => {
                            self.expr(module, x, None);
                        }
                    }
                }
                // With an expected type, that is the list's type. Without one — a bare
                // literal, e.g. a `for` iterable — infer `List[T]` where `T` is the union
                // of the elements' types (`never` for the empty list).
                match expected {
                    Some(t) => t,
                    None => {
                        let elem = if elem_tys.is_empty() {
                            self.env.solver.t.never()
                        } else {
                            self.env.solver.t.union_all(&elem_tys)
                        };
                        // A list literal builds the SAME nominal the declaration does, so
                        // it must use the same qualified identity -- a bare "List" here
                        // would make `[1,2,3]` a different type from `List[i64]`.
                        let name = self.env.solver.t.name(super::env::Env::LIST);
                        self.env.solver.t.nominal(name, vec![elem], vec![])
                    }
                }
            }

            ExprKind::If { cond, then, else_ } => self.if_expr(module, e, cond, then, else_, expected),

            ExprKind::Match { scrutinee, arms } => self.match_expr(module, e, scrutinee, arms, expected),

            ExprKind::Block(b) => self.block(module, b, expected),

            ExprKind::Is { lhs, ty } => {
                let subject = self.expr(module, lhs, None);
                let scope = self.type_scope(module);
                let tested = self.env.resolve(&scope, ty);
                // Testing an opaque value against a structural view is introspection —
                // and worse, in a match arm the test *narrows*, baking the structural
                // atom into the binding's type where the flow gate can no longer tell
                // it from honest knowledge. Naming the record (`is Secret`) stays
                // legal; probing its shape (`is {code: i64}`) does not.
                self.member_gate(
                    module,
                    &e.span,
                    subject,
                    tested,
                    "it can be tested against a structural view",
                );
                // The resolution is the answer lowering needs and cannot compute: the
                // written path is a head name, and `List[i64]` and `List[str]` share one.
                self.result.set_tested(e.id, tested);
                self.env.solver.t.bool()
            }

            ExprKind::As { form, lhs, ty } => {
                let from = self.expr(module, lhs, None);
                let scope = self.type_scope(module);
                let to = self.env.resolve(&scope, ty);
                // The trichotomy (docs/design/checked-casts.md, decision 8). Every cast
                // is one of three classes, and each class has one legal spelling:
                //
                //   always succeeds  — subsumption, widening, newtype wrap/unwrap: bare `as`
                //   might succeed    — the types overlap, neither contains the other: `as?`/`as!`
                //   never succeeds   — no overlap: an error in every spelling
                //
                // The never-class check first: it rejects `as!` too, because an
                // assertion that provably always traps is not a program, it is a typo.
                // Newtype bridges are the one non-overlap exception — a `newtype
                // Meter = f64` is disjoint from `f64`, yet wrapping and unwrapping is
                // exactly what a newtype is for, and neither can fail.
                let meet = self.env.solver.t.intersect(from, to);
                let bridges = self.newtype_bridges(from, to) || self.newtype_bridges(to, from);
                let ok = !self.env.solver.is_empty(meet) || bridges;
                if !self.env.is_error(from) && !ok {
                    let (f, t) = (self.show(from), self.show(to));
                    self.error(e.span.clone(), TypeErrorKind::ImpossibleCast { from: f, to: t });
                    return self.poison();
                }
                // A cast is a flow like any other, but `expr`'s gate never sees it: the
                // legality test above is overlap, not subtyping, and overlap survives
                // sealing — a sealed `Secret` still meets `{code: i64}`, because open
                // records always meet. So the opacity question is asked here in the
                // cast's own terms: sealed, one side must still subsume the other (or a
                // newtype bridge must still hold on sealed representations). `s as
                // Secret` and `s as any` pass — the erasure cancels or the target
                // absorbs it — while `s as {code: i64}` holds only through the contents
                // and is the module reaching inside.
                if !self.env.is_error(from) {
                    if let Some((record, owner, what)) = self.opaque_view(module, from, to) {
                        self.report_opacity(module, e.span.clone(), &record, what, owner);
                        return self.poison();
                    }
                }
                // Infallibility: the always-class is subsumption or a newtype bridge.
                // (Boxing into `any` is subsumption too — `any` is top.)
                let infallible =
                    self.env.solver.is_subtype(from, to) || bridges || self.env.is_error(from);
                match form {
                    ast::CastForm::Plain => {
                        if !infallible {
                            let (f, t) = (self.show(from), self.show(to));
                            self.error(
                                e.span.clone(),
                                TypeErrorKind::FallibleCast { from: f, to: t },
                            );
                            return self.poison();
                        }
                        to
                    }
                    ast::CastForm::Assert => {
                        // The sealed ban: an outsider may not ASSERT another module's
                        // sealed type — an assertion is either redundant (they could
                        // test) or the mistake `sealed` exists to make unrepresentable.
                        // `as?`/`is` stay legal: a test only recognises a value the
                        // owner really built, since tags are stamped at genuine
                        // construction. Fires on the target's head and union leaves;
                        // a caller's own newtype wrapping a sealed type stays legal.
                        if !self.env.is_error(from) {
                            if let Some((record, owner)) = self.sealed_leaf(module, to) {
                                let shown = owner.join("::");
                                self.error(
                                    e.span.clone(),
                                    TypeErrorKind::SealedRecord {
                                        record,
                                        module: shown,
                                        what: "it can be asserted with `as!`".into(),
                                    },
                                );
                                return self.poison();
                            }
                        }
                        // Lowering needs the target to build the runtime test from —
                        // same channel an `is` uses.
                        self.result.set_tested(e.id, to);
                        to
                    }
                    ast::CastForm::Soften => {
                        // `T | null` is only an answer when `null` unambiguously means
                        // "wasn't one" (decision 7).
                        let n = self.env.solver.t.null();
                        let null_meet = self.env.solver.t.intersect(to, n);
                        if !self.env.solver.is_empty(null_meet) {
                            let t = self.show(to);
                            self.error(
                                e.span.clone(),
                                TypeErrorKind::SoftCastNullOverlap { to: t },
                            );
                            return self.poison();
                        }
                        self.result.set_tested(e.id, to);
                        self.env.solver.t.union(to, n)
                    }
                }
            }

            ExprKind::Return(v) => {
                let want = self.ret;
                let t = match v {
                    Some(x) => self.expr(module, x, want),
                    None => self.env.solver.t.tuple(vec![]),
                };
                // Inside a lambda, `return` returns from the *lambda* -- that is what
                // lowering does, since a lambda is lifted to its own function -- so its
                // type joins the lambda's, not the enclosing function's. Checking it
                // against the enclosing function was unsound: a `str` returned through an
                // `i64` slot compiled clean and was reinterpreted.
                if let Some(frame) = self.lambda_returns.last_mut() {
                    frame.push(t);
                }
                self.env.solver.t.never()
            }

            ExprKind::Throw(x) => {
                let t = self.expr(module, x, None);
                self.note_throw(module, x.span.clone(), t, false);
                self.env.solver.t.never()
            }

            ExprKind::Break(v) => {
                // A bare `break` exits with no value, which reads as `null`: a loop
                // that can break bare yields `T | null`, and one that only breaks bare
                // yields `null`.
                let t = match v {
                    Some(x) => self.expr(module, x, None),
                    None => self.env.solver.t.null(),
                };
                match self.loop_breaks.last_mut() {
                    Some(breaks) => breaks.push(t),
                    // No enclosing loop -- either there is genuinely none, or a lambda
                    // sits between here and it, which is the same thing at run time.
                    None => self.error(e.span.clone(), TypeErrorKind::OutsideLoop("break".into())),
                }
                self.env.solver.t.never()
            }
            ExprKind::Continue => {
                if self.loop_breaks.is_empty() {
                    self.error(e.span.clone(), TypeErrorKind::OutsideLoop("continue".into()));
                }
                self.env.solver.t.never()
            }

            ExprKind::While { cond, body } => {
                let b = self.env.solver.t.bool();
                self.expr(module, cond, Some(b));
                // The body runs only while the condition holds, so it sees the
                // then-refinements: `while cur is Node { cur.next }` reads fields off
                // a `Node`, not a `Node | null`. An assignment inside the body checks
                // against the declared type and dissolves the refinement (see Assign).
                let (thens, _) = self.cond_refinements(cond);
                self.loop_breaks.push(vec![]);
                self.with_refinements(&thens, &cond.span, |c| c.block(module, body, None));
                self.loop_breaks.pop();
                self.env.solver.t.tuple(vec![])
            }
            ExprKind::Loop { body } => {
                self.loop_breaks.push(vec![]);
                self.block(module, body, None);
                let breaks = self.loop_breaks.pop().unwrap_or_default();
                if breaks.is_empty() {
                    // No `break` with a value: the loop either never ends or only
                    // breaks bare, so it yields nothing.
                    self.env.solver.t.never()
                } else {
                    self.env.solver.t.union_all(&breaks)
                }
            }
            ExprKind::For { pat, iter, body } => {
                let t = self.expr(module, iter, None);
                let elem = match self.collection_arg(t, 0) {
                    Some(e) => e,
                    None => {
                        if !self.env.is_error(t) {
                            let on = self.show(t);
                            self.error(iter.span.clone(), TypeErrorKind::NotIterable(on));
                        }
                        self.poison()
                    }
                };
                self.locals.push(vec![]);
                self.bind_pattern(module, pat, elem);
                self.loop_breaks.push(vec![]);
                self.block(module, body, None);
                self.loop_breaks.pop();
                self.locals.pop();
                self.env.solver.t.tuple(vec![])
            }

            ExprKind::Assert { args, .. } => {
                for a in args {
                    self.expr(module, a, None);
                }
                self.env.solver.t.tuple(vec![])
            }

            ExprKind::Call { callee, generics, args } => {
                self.call(module, e, callee, generics, args, expected)
            }

            ExprKind::Field { base, name } => {
                let t = self.expr(module, base, None);
                self.check_opacity(module, e.span.clone(), t, name);
                let label = self.env.solver.t.name(name);
                let p = narrow::project_field(&mut self.env.solver, t, label);
                self.projected(e.span.clone(), p, name, t)
            }

            ExprKind::Error => self.poison(),

            ExprKind::Lambda { params, body } => self.lambda(module, e, params, body, expected),

            ExprKind::RecordLit { path, fields, spread } => {
                self.record_lit(module, e, path, fields, spread, expected)
            }

            ExprKind::Index { base, index } => {
                let t = self.expr(module, base, None);
                // A two-argument collection -- `Map[K, V]` -- is keyed by K (#0) and
                // yields V (#1). A one-argument `List[T]` is keyed by i64 and yields T.
                let arg1 = self.collection_arg(t, 1);
                let (key, value) = match arg1 {
                    Some(v) => (self.collection_arg(t, 0), Some(v)),
                    None => (Some(self.env.solver.t.i64()), self.collection_arg(t, 0)),
                };
                if let Some(k) = key {
                    self.expr(module, index, Some(k));
                } else {
                    self.expr(module, index, None);
                }
                match value {
                    Some(v) => v,
                    None => {
                        if !self.env.is_error(t) {
                            let on = self.show(t);
                            self.error(e.span.clone(), TypeErrorKind::NotIndexable(on));
                        }
                        self.poison()
                    }
                }
            }

            ExprKind::Try { form, body, catch } => {
                self.try_expr(module, e.id, *form, body, catch, expected)
            }
        }
    }

    /// A lambda, in checking mode. Its parameter types come from their annotations,
    /// or from the expected arrow flowing in — `map(xs, (x) => x + 1)` gets `x: i64`
    /// from `map`'s parameter. A parameter with neither is an error, not a guess:
    /// inferring it from a later use, or from the body, is unification, which this
    /// bidirectional checker does not do. See `decisions.md` on Castagna.
    fn lambda(
        &mut self,
        module: &[String],
        e: &Expr,
        params: &[ast::LambdaParam],
        body: &Expr,
        expected: Option<TyId>,
    ) -> TyId {
        let scope = self.type_scope(module);
        let want = expected.and_then(|t| self.env.solver.t.as_arrow(t));

        // Everything already on the stack is captured; the lambda's own scope starts
        // here. An assignment to a name below this floor is a rebind of a capture.
        self.capture_floors.push(self.locals.len());
        self.locals.push(vec![]);
        let mut param_tys = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            let t = match (&p.ty, want.as_ref().and_then(|a| a.params.get(i))) {
                (Some(spec), _) => self.env.resolve(&scope, spec),
                (None, Some(&pt)) => pt,
                (None, None) => {
                    self.error(e.span.clone(), TypeErrorKind::LambdaParamNeedsType(p.name.clone()));
                    self.poison()
                }
            };
            // A lambda param carries no span of its own; the lambda's is close enough
            // for a diagnostic, and a param is never a capture anyway.
            self.bind(&p.name, t, e.span.clone(), DefKind::Param);
            param_tys.push(t);
        }

        // A lambda body is a new *function* context, and every function-scoped thing has
        // to be reset for it -- not just `throws`. Leaving the rest in place let control
        // flow escape a boundary it cannot actually cross at run time, because the lambda
        // is lifted into its own function:
        //
        //   `return`      was checked against the enclosing function's return type, so a
        //                 `str` could be returned through an `i64` slot. Unsound.
        //   `throw`       was absorbed by an enclosing `try`, so the checker called an
        //                 error handled that escapes uncaught at run time.
        //   `break`       resolved to an enclosing loop, and reached `unreachable`.
        //
        // A lambda cannot *declare* `throws` — there is no syntax — but it never needed
        // to: parameters are inputs and need a source, while the return type and throws
        // are outputs, always derivable from the body. So the body is checked in `Infer`
        // mode and whatever propagates out is collected.
        let want_ret = want.as_ref().map(|a| a.ret);
        let never = self.env.solver.t.never();
        let saved_throws = self.throws.replace(Throws::Infer);
        let saved_ret = self.ret.take();
        let saved_sinks = std::mem::take(&mut self.throw_sinks);
        let saved_breaks = std::mem::take(&mut self.loop_breaks);
        self.ret = want_ret;
        self.lambda_returns.push(vec![]);
        self.lambda_throws.push(vec![]);

        let tail = self.expr(module, body, want_ret);

        let returned = self.lambda_returns.pop().unwrap_or_default();
        let thrown = self.lambda_throws.pop().unwrap_or_default();
        self.throws = saved_throws;
        self.ret = saved_ret;
        self.throw_sinks = saved_sinks;
        self.loop_breaks = saved_breaks;
        self.locals.pop();
        self.capture_floors.pop();

        // The lambda's return type is its tail unioned with whatever its `return`s give.
        let ret = returned.into_iter().fold(tail, |acc, t| self.union_branches(acc, t));

        // Its `throws` is what the body propagates — plus the expected arrow's clause,
        // when one flows in. Adopting the clause matters beyond subtyping: the clause is
        // part of the calling convention (a throwing closure returns a tagged result), so
        // a lambda filling a `(i64) throws E -> i64` slot must *be* one, even when its
        // own body cannot fail. Widening the throws is free at creation and the body's
        // errors still have to fit the clause, checked by `expr`'s assignability.
        let mut throws = thrown.into_iter().fold(never, |acc, t| self.union_branches(acc, t));
        if let Some(a) = &want {
            throws = self.union_branches(throws, a.throws);
        }

        let arrow = self.env.solver.t.arrow(param_tys, throws, ret);
        self.result.set_lambda(e.id, arrow);
        arrow
    }

    /// A record literal, in one of three modes depending on what is known about it: named
    /// (`Point { .. }`), anonymous against a known target, and anonymous with no target.
    ///
    /// Only the last synthesizes a structural type from the fields. The first two check
    /// against a set of declared fields, and that is deliberate — the excess-field rule
    /// (a field the target does not declare is an error) is only sound for a *fresh
    /// literal*, where an unexpected name is a typo. The same record held in a variable
    /// still widens by ordinary width subtyping, because there the extra field may be
    /// exactly what its owner wanted. This is TypeScript's split, and it is why a literal
    /// is not simply synthesized and then checked like everything else.
    ///
    /// A named path that fails to resolve, or resolves to something that is not a single
    /// record, still walks the field values (so mistakes inside them are reported) and
    /// then falls back to the expected type or poison rather than inventing a shape.
    fn record_lit(
        &mut self,
        module: &[String],
        e: &Expr,
        path: &Option<Vec<String>>,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
        expected: Option<TyId>,
    ) -> TyId {
        // A named literal builds a nominal record. Resolve the type, then check its
        // fields exactly as an anonymous literal is checked against a target: every
        // field declared, right types, no extras. Generic records need their
        // arguments inferred from the fields, which is not built yet -- those still
        // flow the expected type unchecked.
        if let Some(p) = path {
            // Building one — with or without a spread, which is an update and so equally
            // a way to set a field the module means to control.
            let what = if spread.is_some() { "it can be updated" } else { "it can be built" };
            self.check_opaque_path(module, e.span.clone(), p, what);
            let key = self.env.lookup(module, p);
            if let Some(key) = &key {
                if self.env.is_generic(key) {
                    return self.generic_record_lit(module, e, key, fields, spread);
                }
                let scope = self.type_scope(module);
                let spec = ast::TypeSpec {
                    kind: ast::TypeSpecKind::Named { path: p.clone(), args: vec![] },
                    span: e.span.clone(),
                };
                let record_ty = self.env.resolve(&scope, &spec);
                if let Some(target_fields) = self.record_fields(record_ty) {
                    self.check_record_fields(module, e, fields, spread, &target_fields);
                    return record_ty;
                }
            }
            for f in fields {
                self.expr(module, &f.value, None);
            }
            if let Some(s) = spread {
                self.expr(module, s, None);
            }
            return expected.unwrap_or_else(|| self.poison());
        }

        // An anonymous record. A fresh literal is checked exactly against the type
        // it is written for: excess fields the target does not declare are an error
        // (a typo, not a widening), while a missing nullable field is fine. A record
        // held in a variable still flows by width subtyping -- this excess check is
        // TypeScript's, and it is why a literal differs from a value here.
        if let Some(target_fields) = expected.and_then(|exp| self.record_fields(exp)) {
            // An anonymous literal filling a nominal expected type *builds* that
            // nominal — the same act as writing its name, minus the name. For a
            // foreign opaque record that is forgery by annotation (`let s:
            // vault::Secret = { code: 99 }`), and it never passes the flow gate:
            // this branch returns the expected type itself, so `expr` compares
            // `Secret` to `Secret`. Ask by the target's name here, exactly as the
            // named-literal path above does.
            if let Some(exp) = expected {
                for n in self.nominal_leaves(exp) {
                    self.check_opaque_name(module, e.span.clone(), &n, "it can be built");
                }
            }
            self.check_record_fields(module, e, fields, spread, &target_fields);
            return expected.expect("target present");
        }

        let mut seen: Vec<String> = Vec::new();
        let mut field_tys: Vec<(super::types::NameId, TyId)> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            let t = self.expr(module, &f.value, None);
            let label = self.env.solver.t.name(&f.name);
            field_tys.push((label, t));
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
        }
        self.env.solver.t.struct_ty(field_tys)
    }

    /// Check a literal's fields against a record's declared fields: each present and
    /// declared (no extras), each typed, and no required (non-nullable) field missing.
    fn check_record_fields(
        &mut self,
        module: &[String],
        e: &Expr,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
        target: &[(String, TyId)],
    ) {
        let mut seen: Vec<String> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            match target.iter().find(|(n, _)| *n == f.name) {
                Some((_, want)) => {
                    // Like `expr`, but a mismatch names the field rather than reporting a
                    // bare type pair -- `{ timeout: "x" }` should point at `timeout`.
                    let want = *want;
                    let got = self.infer(module, &f.value, Some(want));
                    self.result.set_ty(f.value.id, got);
                    if !self.assignable(module, &f.value.span, got, want) {
                        let (expected, found) = (self.show(want), self.show(got));
                        self.error(
                            f.value.span.clone(),
                            TypeErrorKind::FieldTypeMismatch { field: f.name.clone(), expected, found },
                        );
                    }
                }
                None => {
                    self.expr(module, &f.value, None);
                    let on = self.record_name(target);
                    self.error(f.span.clone(), TypeErrorKind::NoField { field: f.name.clone(), on });
                }
            }
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
            return;
        }
        for (name, fty) in target {
            if !seen.contains(name) && !self.is_nullable(*fty) {
                self.error(e.span.clone(), TypeErrorKind::MissingField(name.clone()));
            }
        }
    }

    /// A named literal for a generic record: `Box { item: 1 }`. Instantiate the
    /// record with fresh rigid variables, infer them from the field values, then
    /// substitute -- so the literal's type is `Box[i64]`, not `Box[T]`.
    fn generic_record_lit(
        &mut self,
        module: &[String],
        e: &Expr,
        key: &str,
        fields: &[ast::FieldInit],
        spread: &Option<Box<Expr>>,
    ) -> TyId {
        use std::collections::{HashMap, HashSet};
        let names = self.env.generic_names(key);
        let var_names: HashSet<_> = names.iter().map(|n| self.env.solver.t.name(n)).collect();
        let var_args: Vec<TyId> = names
            .iter()
            .map(|n| {
                let nn = self.env.solver.t.name(n);
                self.env.solver.t.var(nn)
            })
            .collect();
        let templated = self.env.instantiate(key, var_args, &e.span);
        let tfields = self.record_fields(templated).unwrap_or_default();

        // Infer the variables from the fields, remembering each field's own type.
        let mut subst: HashMap<_, TyId> = HashMap::new();
        let mut given: Vec<(String, TyId)> = Vec::new();
        let mut seen: Vec<String> = Vec::new();
        for f in fields {
            if seen.contains(&f.name) {
                self.error(f.span.clone(), TypeErrorKind::DuplicateField(f.name.clone()));
            }
            seen.push(f.name.clone());
            let ft = self.expr(module, &f.value, None);
            match tfields.iter().find(|(n, _)| *n == f.name) {
                Some((_, tmpl)) => {
                    super::generic::infer(&mut self.env.solver.t, *tmpl, ft, &var_names, &mut subst);
                    given.push((f.name.clone(), ft));
                }
                None => {
                    let on = key.rsplit("::").next().unwrap_or(key).to_string();
                    self.error(f.span.clone(), TypeErrorKind::NoField { field: f.name.clone(), on });
                }
            }
        }
        if let Some(s) = spread {
            self.expr(module, s, None);
        }

        // Now check each field against the resolved parameter type -- this catches a
        // variable pinned by one field and violated by another, e.g. Pair[T] with
        // mismatched a and b.
        for (name, got) in &given {
            if let Some((_, tmpl)) = tfields.iter().find(|(n, _)| n == name) {
                let want = self.env.solver.t.substitute(*tmpl, &subst);
                if !self.assignable(module, &e.span, *got, want) {
                    let (g, w) = (self.show(*got), self.show(want));
                    self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: w, found: g });
                }
            }
        }
        if spread.is_none() {
            for (name, tmpl) in &tfields {
                if !seen.contains(name) {
                    let concrete = self.env.solver.t.substitute(*tmpl, &subst);
                    if !self.is_nullable(concrete) {
                        self.error(e.span.clone(), TypeErrorKind::MissingField(name.clone()));
                    }
                }
            }
        }
        self.env.solver.t.substitute(templated, &subst)
    }

    /// What to call the target in a "no such field" diagnostic. By this point the target
    /// is a bare field list with no name attached, so it is described by its labels --
    /// `{x, y}` -- which is at least something the reader can match against what they
    /// wrote. Not a printed type: the field *types* are noise in this message.
    fn record_name(&mut self, target: &[(String, TyId)]) -> String {
        let fs: Vec<String> = target.iter().map(|(n, _)| n.clone()).collect();
        format!("{{{}}}", fs.join(", "))
    }

    /// Whether `null` is one of the values `ty` admits, and so whether a field of this
    /// type may be left out of a literal. Read straight off the base bits rather than
    /// asked as a subtyping question, which keeps it cheap and exact for the one thing it
    /// is used for.
    fn is_nullable(&self, ty: TyId) -> bool {
        self.env.solver.t.data(ty).base & super::types::B_NULL != 0
    }

    /// A type that is only atoms -- `:ok`, `:ok | :err`. All atoms share one
    /// comparison domain, so two of them may be compared for equality.
    fn is_atomic(&self, ty: TyId) -> bool {
        let d = self.env.solver.t.data(ty);
        d.base == 0
            && self.env.solver.t.atomset_of(d.vars).is_empty_set()
            && !self.env.solver.t.atomset_of(d.atoms).is_empty_set()
            && d.records == super::bdd::FALSE
            && d.tuples == super::bdd::FALSE
            && d.arrows == super::bdd::FALSE
    }

    /// A type with a structural order, given whatever `where T: Ord` bounds are in scope.
    ///
    /// The rule itself lives in `typecheck::ordered`, because the `marker Ord` bound needs
    /// the same answer when it is discharged at a call site. See that module for why order
    /// is infectious and why the bound set is threaded through the recursion.
    fn is_ordered(&self, ty: TyId) -> bool {
        super::ordered::is_ordered(self.env, ty, &self.ord_bound_vars())
    }

    /// The type parameters this signature declared `where T: Ord` for.
    fn ord_bound_vars(&self) -> std::collections::HashSet<String> {
        self.bounds
            .iter()
            .filter(|(_, p)| {
                let proto = self.env.protocol(*p);
                proto.is_marker && proto.name == "Ord"
            })
            .map(|(n, _)| n.clone())
            .collect()
    }

    /// Record that something throws `throws`. Inside a `try` it lands in the sink to
    /// be caught or propagated; from a call outside any `try` it is a bare throwing
    /// call, a compile error; from a `throw` statement outside a `try` it propagates
    /// to the enclosing function's declared `throws`.
    fn note_throw(&mut self, module: &[String], span: Span, throws: TyId, from_call: bool) {
        let never = self.env.solver.t.never();
        if self.env.is_error(throws) || throws == never {
            return;
        }
        if let Some(sink) = self.throw_sinks.last_mut() {
            sink.push(throws);
        } else if from_call {
            self.error(span, TypeErrorKind::BareThrowingCall);
        } else {
            match self.throws {
                // Escaping `main`: it must be reportable, so it must implement `Error`.
                // Resolving `message` for it is the check — and it answers for a union by
                // requiring every variant to have an impl.
                Some(Throws::ImplicitError) => {
                    if !self.implements_error(throws) {
                        let t = self.show(throws);
                        self.error(span, TypeErrorKind::NotAnError { thrown: t });
                    }
                }
                // A lambda body: nothing to check against — the escape *becomes* part
                // of the lambda's inferred `throws`.
                Some(Throws::Infer) => {
                    if let Some(frame) = self.lambda_throws.last_mut() {
                        frame.push(throws);
                    }
                }
                other => {
                    let want = match other {
                        Some(Throws::Declared(t)) => t,
                        _ => never,
                    };
                    if !self.assignable(module, &span, throws, want) {
                        let (t, w) = (self.show(throws), self.show(want));
                        self.error(span, TypeErrorKind::Throws { thrown: t, declared: w });
                    }
                }
            }
        }
    }

    /// Whether a type implements `Error`. Asking dispatch to resolve `message` for it is
    /// the whole check: it succeeds only when every value the type admits has an impl.
    fn implements_error(&mut self, ty: TyId) -> bool {
        let Some(proto) = self.env.lookup_protocol(&[], &["Error".to_string()]) else {
            return true; // no prelude in scope; nothing to enforce
        };
        dispatch::resolve(self.env, "message", Some(proto), &[ty], None).is_ok()
    }

    /// `try`, in all four shapes: with a `catch`, or as one of the propagate / soften /
    /// assert forms.
    ///
    /// The sink is what makes this work. Pushing a frame onto `throw_sinks` means every
    /// throwing call in the body lands here instead of being checked against the enclosing
    /// function's clause, and the union of what landed is the error type. That union is
    /// recorded against the expression id so lowering can give the handler a concrete
    /// parameter rather than an erased one.
    ///
    /// A body that cannot fail leaves the sink empty and the bound error type `never`,
    /// which is honest: there is no value for the catch to receive.
    fn try_expr(
        &mut self,
        module: &[String],
        id: crate::ast::ExprId,
        form: ast::TryForm,
        body: &Expr,
        catch: &Option<ast::CatchArm>,
        expected: Option<TyId>,
    ) -> TyId {
        self.throw_sinks.push(vec![]);
        // The body's value flows the expected type down only when nothing follows it;
        // with a catch the arms are unioned, so let both synthesize.
        let val = self.expr(module, body, if catch.is_some() { None } else { expected });
        let thrown = self.throw_sinks.pop().unwrap_or_default();
        let caught = self.env.solver.t.union_all(&thrown);
        let never = self.env.solver.t.never();

        // Hand the exact error type to lowering, so the handler's parameter is concrete
        // rather than erased.
        let handled = if thrown.is_empty() { never } else { caught };
        self.result.set_caught(id, handled);

        if let Some(arm) = catch {
            // The error union is handled here, not propagated. `catch` binds it.
            self.locals.push(vec![]);
            let bound = if thrown.is_empty() { never } else { caught };
            self.bind(&arm.binding, bound, arm.span.clone(), DefKind::Local);
            let handled = self.block(module, &arm.body, expected);
            self.locals.pop();
            return self.union_branches(val, handled);
        }

        match form {
            // Propagate: the errors become the enclosing function's to declare.
            ast::TryForm::Propagate => {
                self.note_throw(module, body.span.clone(), caught, false);
                val
            }
            // Soften: a failure yields null instead.
            ast::TryForm::Soften => {
                let null = self.env.solver.t.null();
                self.env.solver.t.union(val, null)
            }
            // Assert: a failure panics, so the value is the success type unchanged.
            ast::TryForm::Assert => val,
        }
    }

    /// The element of a single-argument collection -- `List[T]` carries it in `#0`.
    /// `for x in xs` and `xs[i]` both read it.
    fn element_type(&mut self, ty: TyId) -> Option<TyId> {
        self.arg_type(ty, 0)
    }

    /// A generic argument by position: `#0`, `#1`. `None` when the type has no such
    /// slot, which is how a `Map` (two arguments) is told from a `List` (one).
    fn arg_type(&mut self, ty: TyId, i: usize) -> Option<TyId> {
        let label = self.env.solver.t.arg_label(i);
        narrow::project_field(&mut self.env.solver, ty, label).ty()
    }

    /// `arg_type`, but only for a subject that could actually *be* a collection.
    ///
    /// `arg_type` alone is not that question. It reads the slot off every record leaf of
    /// the type and answers `Partial` where some leaf lacks it — and `any` has a record
    /// leaf, so `element_type(any)` answered `Some(any)`. That made `for x in v` compile
    /// for a `v: any` holding an i64, and the backend read the scalar as a list:
    ///
    /// ```text
    ///     let v: any = 5; for x in v { .. }     // ASan: heap-buffer-overflow
    /// ```
    ///
    /// A collection is a nominal record and nothing else. A type that also admits an i64,
    /// a str, an atom, a tuple, a function, `null`, or a rigid variable is not one,
    /// whatever its record leaves happen to carry — so those are rejected here rather
    /// than silently yielding a plausible element type.
    fn collection_arg(&mut self, ty: TyId, i: usize) -> Option<TyId> {
        if !self.is_all_records(ty) {
            return None;
        }
        self.arg_type(ty, i)
    }

    /// A type built out of records and nothing else. Mirrors `is_atomic`, which asks the
    /// same shape of question for the atom lattice.
    fn is_all_records(&self, ty: TyId) -> bool {
        let d = self.env.solver.t.data(ty);
        d.base == 0
            && self.env.solver.t.atomset_of(d.atoms).is_empty_set()
            && self.env.solver.t.atomset_of(d.vars).is_empty_set()
            && d.tuples == super::bdd::FALSE
            && d.arrows == super::bdd::FALSE
            && d.records != super::bdd::FALSE
    }

    /// Reject reaching inside an `opaque` record from outside the module that owns it.
    ///
    /// The three ways in are reading a field, building a literal, and destructuring a
    /// pattern; all of them land here. Holding and passing a value are deliberately not
    /// among them — opacity hides the contents, not the type.
    ///
    /// See `opacity_permits` for which modules count as inside.
    fn check_opaque_name(&mut self, module: &[String], span: Span, name: &str, what: &str) {
        let Some(owner) = self.env.opaque_record_named(module, name) else { return };
        let owner = owner.to_vec();
        self.report_opacity(module, span, name, what, owner);
    }

    /// The same rule for a record the source *names*, where the written path resolves
    /// unambiguously and no fallback is needed.
    fn check_opaque_path(&mut self, module: &[String], span: Span, path: &[String], what: &str) {
        let Some(owner) = self.env.opaque_record_at(module, path) else { return };
        let owner = owner.to_vec();
        let name = path.last().cloned().unwrap_or_default();
        self.report_opacity(module, span, &name, what, owner);
    }

    /// The visibility rule itself, shared by both entry points so they cannot drift.
    ///
    /// `owner` is the module that declared the record. Access is allowed from that module
    /// and from anything **nested inside** it — its subtree — and nowhere else: not from a
    /// sibling, not from the root, and **not from its parent**. `opacity_permits` is where
    /// the two cases are spelled out and justified.
    ///
    /// One direction carries the weight, and it is the *outward* one: `std::fs`'s
    /// `internal mod raw` reaches out to build the `File` its parent declares. This doc
    /// used to claim the inward direction too — a parent reaching into its child — and
    /// credited `std::fs` with it; that was simply wrong about which branch the stdlib
    /// uses, and the branch has since been removed.
    ///
    /// An empty owner path is the prelude, which has no name to print.
    fn report_opacity(
        &mut self,
        module: &[String],
        span: Span,
        name: &str,
        what: &str,
        owner: Vec<String>,
    ) {
        if super::env::opacity_permits(module, &owner) {
            return;
        }
        let shown = if owner.is_empty() { "the prelude".to_string() } else { owner.join("::") };
        self.error(
            span,
            TypeErrorKind::OpaqueRecord {
                record: name.to_string(),
                module: shown,
                what: what.to_string(),
            },
        );
    }

    /// The field-read entry point: the record is whatever the base expression turned out
    /// to be, so the name has to come from the type rather than from syntax.
    ///
    /// Every nominal *leaf* of the type is checked, not just the single-atom case a
    /// `nominal_of` would catch. A union (`Secret | {code: i64}`) and an intersection
    /// (what a narrowed match arm binds) both project the field across their record
    /// leaves, so a foreign opaque record anywhere among them makes the read an
    /// introspection — reading around it would still reveal which fields it has.
    fn check_opacity(&mut self, module: &[String], span: Span, ty: TyId, field: &str) {
        for name in self.nominal_leaves(ty) {
            self.check_opaque_name(
                module,
                span.clone(),
                &name,
                &format!("its field `{field}` is readable"),
            );
        }
    }

    /// The nominal names appearing positively among a type's record leaves — one per
    /// distinct `#nominal` singleton tag, across every union arm and intersection part.
    fn nominal_leaves(&self, ty: TyId) -> Vec<String> {
        let t = &self.env.solver.t;
        let d = t.data(ty);
        let mut out: Vec<String> = Vec::new();
        for (pos, _) in t.rec_bdd.paths(d.records) {
            for i in pos {
                let tag = t.rec_atoms[i as usize].get(t.nominal_label);
                let atoms = t.atomset_of(t.data(tag).atoms);
                if !atoms.neg && atoms.names.len() == 1 {
                    let n = t.name_str(atoms.names[0]).to_string();
                    if !out.contains(&n) {
                        out.push(n);
                    }
                }
            }
        }
        out
    }

    /// The declared fields of a record type -- the user-written ones, dropping the
    /// reserved `#nominal` and `#0`, `#1` generic-argument slots. `None` when `ty`
    /// is not a single record atom.
    fn record_fields(&self, ty: TyId) -> Option<Vec<(String, TyId)>> {
        let t = &self.env.solver.t;
        let d = t.data(ty);
        match t.rec_bdd.paths(d.records).as_slice() {
            [(pos, neg)] if neg.is_empty() && pos.len() == 1 => {
                let atom = &t.rec_atoms[pos[0] as usize];
                Some(
                    atom.fields
                        .iter()
                        .map(|&(l, ft)| (t.name_str(l).to_string(), ft))
                        .filter(|(n, _)| !n.starts_with('#'))
                        .collect(),
                )
            }
            _ => None,
        }
    }

    /// A name in value position, resolved local-then-function. A single-segment path may
    /// be a local, and a local shadows a function of the same name; anything qualified
    /// can only be a function.
    ///
    /// Note the order of the two failure paths: a name that exists but is `internal` to
    /// another module reports *that*, before the generic "not in scope". Reporting the
    /// generic one first sent people looking for a typo in a name they had spelled
    /// correctly.
    fn path(&mut self, module: &[String], e: &Expr, p: &[String]) -> TyId {
        if let [one] = p {
            if let Some(t) = self.lookup(one) {
                // The one place in the compiler that knows this `one` is *that* one.
                // Recorded rather than recomputed because no later pass can: the frame
                // that held the binding is popped when the block ends.
                if let Some((span, kind)) = self.binding_of(one) {
                    self.result.set_def(e.id, DefSite { module: module.to_vec(), span, kind });
                }
                return t;
            }
        }
        let joined = p.join("::");
        // Before functions, after locals: a local shadows a const exactly as it shadows a
        // fn, and a const and a fn of one name in one module is a collision the author
        // should fix rather than one this silently orders.
        if let Some(c) = self.env.const_named(module, p) {
            let ty = c.ty;
            let site =
                DefSite { module: c.module.clone(), span: c.span.clone(), kind: DefKind::Const };
            self.result.set_def(e.id, site);
            return ty;
        }
        if let Some(sig) = self.env.fn_named(module, p) {
            // A function used as a value, throwing or not: its arrow carries the
            // `throws`, the closure repr carries it in turn, and the adapter thunk
            // returns the tagged result — the calling convention survives the trip.
            //
            // The declaring module, not `module`: a `use`d name is defined where it was
            // written, and that is the file the jump has to open.
            let ty = sig.ty;
            let site =
                DefSite { module: sig.module.clone(), span: sig.span.clone(), kind: DefKind::Fn };
            self.result.set_def(e.id, site);
            return ty;
        }
        // A name that exists but is fenced off reports why, rather than "not in scope".
        if let Some(owner) = self.env.hidden_by_internal(module, p) {
            self.error(e.span.clone(), TypeErrorKind::Internal { name: joined, owner });
            return self.poison();
        }
        self.error(e.span.clone(), TypeErrorKind::UnknownName(joined));
        self.poison()
    }

    /// Binary operators. Most of them are not really "binary" at the type level: `|>`
    /// rewrites into a call, `and`/`or` demand `bool` on both sides, `orelse` performs a
    /// set subtraction, and only the arithmetic and bitwise fallthrough treats the two
    /// operands as the same type.
    ///
    /// That fallthrough accepts any pair where one side is assignable to the other and
    /// yields the *left* type. It does not consult a numeric protocol, so `str + i64` is
    /// caught only because the two are unrelated, not because addition was checked.
    fn binary(&mut self, module: &[String], e: &Expr, op: BinOp, lhs: &Expr, rhs: &Expr, expected: Option<TyId>) -> TyId {
        match op {
            BinOp::And | BinOp::Or => {
                let b = self.env.solver.t.bool();
                self.expr(module, lhs, Some(b));
                self.expr(module, rhs, Some(b));
                b
            }
            // Equality needs comparable operands: they overlap, or both are atoms
            // (which form one comparison domain). `:ok == :err` is false, but
            // `:ok == "ok"` compares an atom to a string, which is a mistake.
            BinOp::Eq | BinOp::Ne => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                let is_null = |e: &Expr| matches!(e.kind, ExprKind::Null);
                if !is_null(lhs) && !is_null(rhs) && !self.env.is_error(l) && !self.env.is_error(r) {
                    let meet = self.env.solver.t.intersect(l, r);
                    let both_atoms = self.is_atomic(l) && self.is_atomic(r);
                    // One side being a subtype of the other is comparable even when the
                    // meet is empty: `xs == []` compares `List[i64]` with `List[never]`,
                    // and `List[never]` has no *inhabitants* to intersect with, so the
                    // overlap test alone rejected the natural way to ask "is this empty".
                    let related = self.env.solver.is_subtype(l, r) || self.env.solver.is_subtype(r, l);
                    if self.env.solver.is_empty(meet) && !both_atoms && !related {
                        let (a, b) = (self.show(l), self.show(r));
                        self.error(e.span.clone(), TypeErrorKind::Incomparable { left: a, right: b });
                    } else if !super::ordered::is_equatable(self.env, l)
                        || !super::ordered::is_equatable(self.env, r)
                    {
                        // Both sides, not the meet: `Map[str, i64] == Map[str, i64]` meets
                        // itself, and it is the operands the backend has to compare.
                        let ty = self.show(l);
                        self.error(e.span.clone(), TypeErrorKind::Unequatable { ty });
                    }
                }
                self.env.solver.t.bool()
            }
            // Ordering needs an order. `1 < 2`, `"a" < "b"` and two of the same record are
            // fine; `1 < "s"` has no common type, and a union, an atom or a function has
            // no order even when both sides have the same type.
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                let meet = self.env.solver.t.intersect(l, r);
                if !self.env.is_error(l) && !self.env.is_error(r) {
                    if self.env.solver.is_empty(meet) {
                        let (a, b) = (self.show(l), self.show(r));
                        self.error(e.span.clone(), TypeErrorKind::Incomparable { left: a, right: b });
                    } else if !self.is_ordered(meet) {
                        let shown = self.show(meet);
                        self.error(e.span.clone(), TypeErrorKind::Unordered { ty: shown });
                    } else if let Some((record, owner)) = self.sealed_leaf(module, meet) {
                        // The Ord bar (docs/design/checked-casts.md, decision 2;
                        // opacity.md residue 2): ordering foreign sealed values is an
                        // oracle — with chosen seals in hand, relative order supports
                        // binary search of the hidden contents. `==` stays: identity
                        // of contents is one bit per comparison.
                        let shown = owner.join("::");
                        self.error(
                            e.span.clone(),
                            TypeErrorKind::SealedRecord {
                                record,
                                module: shown,
                                what: "it can be ordered".into(),
                            },
                        );
                    }
                }
                self.env.solver.t.bool()
            }
            BinOp::Orelse => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                // `orelse` replaces the null arm, so the result is the rest of the
                // left plus the right.
                let null = self.env.solver.t.null();
                let non_null = self.env.solver.t.diff(l, null);
                self.env.solver.t.union(non_null, r)
            }
            BinOp::Pipe => {
                // `a |> f(b)` is `f(a, b)`: the receiver becomes the first argument.
                if let ExprKind::Call { callee, generics, args } = &rhs.kind {
                    let mut piped = Vec::with_capacity(args.len() + 1);
                    piped.push(lhs.clone());
                    piped.extend(args.iter().cloned());
                    return self.call(module, rhs, callee, generics, &piped, expected);
                }
                // `a |> f` with a bare callee applies it to the receiver.
                let f = self.expr(module, rhs, None);
                self.apply(module, e, "the right of `|>`", f, std::slice::from_ref(lhs))
            }
            _ => {
                let l = self.expr(module, lhs, None);
                let r = self.expr(module, rhs, None);
                if !self.assignable(module, &e.span, r, l) && !self.assignable(module, &e.span, l, r) {
                    let (a, b) = (self.show(l), self.show(r));
                    self.error(e.span.clone(), TypeErrorKind::Mismatch { expected: a, found: b });
                    return self.poison();
                }
                l
            }
        }
    }

    /// An `if`. With both arms it is the union of them; with only a `then` it yields
    /// `()`, and whether that is acceptable is decided against the expected type here
    /// rather than by the caller.
    ///
    /// The expected type flows into *both* arms, not into their union afterwards. That is
    /// what lets each arm use it — a lambda in one arm gets its parameter types — and it
    /// means an arm is reported against what was asked for rather than against whatever
    /// the other arm happened to produce.
    fn if_expr(
        &mut self,
        module: &[String],
        e: &Expr,
        cond: &Expr,
        then: &ast::Block,
        else_: &Option<Box<Expr>>,
        expected: Option<TyId>,
    ) -> TyId {
        let b = self.env.solver.t.bool();
        self.expr(module, cond, Some(b));
        // `if x is T { .. }` sees `x` at `T` in the then-branch and at the complement
        // in the else-branch; same for null tests and their and/or/not compositions.
        let (thens, elses) = self.cond_refinements(cond);

        let Some(other) = else_ else {
            self.with_refinements(&thens, &cond.span, |c| c.block(module, then, None));
            // With no `else`, the `if` yields `()` when the condition is false. That is
            // fine wherever `()` is accepted -- a statement, or a `-> null`/`-> ()` tail
            // -- and an error only where a real value is required.
            let unit = self.env.solver.t.tuple(vec![]);
            let rejects_unit = expected.is_some_and(|exp| !self.env.solver.is_subtype(unit, exp));
            if rejects_unit {
                self.error(e.span.clone(), TypeErrorKind::IfWithoutElse);
                return self.poison();
            }
            return unit;
        };

        let a = self.with_refinements(&thens, &cond.span, |c| c.block(module, then, expected));
        let c = self.with_refinements(&elses, &cond.span, |ch| ch.expr(module, other, expected));
        self.union_branches(a, c)
    }

    /// A `match`: narrowing, binding and exhaustiveness in one left-to-right pass.
    ///
    /// The whole thing turns on `remaining`, the residual of the subject after the arms
    /// above have peeled off what they cover. An arm binds against `remaining ∧ its
    /// test`, not against the full subject, which is why a bare binding following an
    /// `is null` arm receives the non-null half; and exhaustiveness is not a separate
    /// analysis but simply `remaining` having gone empty by the end, with whatever is
    /// left over naming exactly the values no arm handled.
    ///
    /// Only an exact, unguarded arm subtracts anything — see `narrow::Test`. `bool` needs
    /// a special case on top of that, because it is a single base bit rather than
    /// `:true | :false`, so a `true` arm's type is the whole of `bool` and subtracting it
    /// would wrongly exhaust the type after one arm. The two literals are tracked by hand
    /// instead and `bool` is subtracted only once both have been seen unguarded.
    fn match_expr(
        &mut self,
        module: &[String],
        e: &Expr,
        scrutinee: &Expr,
        arms: &[ast::MatchArm],
        expected: Option<TyId>,
    ) -> TyId {
        let subject = self.expr(module, scrutinee, None);
        // A bare-variable scrutinee is re-narrowed inside each arm, so `is Circle`
        // makes `s.r` legal: the arm sees `s` as the member, not the whole union.
        let scrut_var = self.scrutinee_var(scrutinee);
        let mut result = self.env.solver.t.never();

        // The running residual: what values could still reach this arm, given the
        // arms above it already peeled off theirs. Narrowing against it — not the
        // full subject — is why a bare binding after `is null` receives the non-null
        // half, and why exhaustiveness falls out as `remaining` reaching empty.
        let mut remaining = subject;
        // `bool` is one base bit, not `:true | :false`, so a boolean literal types
        // as the whole `bool` and cannot be subtracted precisely. Track the two
        // values by hand: seeing both, unguarded, exhausts `bool`.
        let (mut saw_true, mut saw_false) = (false, false);
        for arm in arms {
            let test = self.arm_test(module, arm, subject);
            self.locals.push(vec![]);
            // An arm whose test is disjoint from the *subject* can never run, and binding
            // it is the trap `narrow.rs` was built to prevent (see its module docs, and
            // `Refined::NeverMatches`). `remaining ∧ test` would be `never`, `never` is
            // below everything, so every check inside the arm succeeds vacuously:
            //
            //   fn f[T](x: T) -> str { match x { is i64 => g(x), _ => "no" } }
            //
            // A rigid `T` is disjoint from `i64`, so `x` bound `never` and `g(x: str)`
            // typechecked. `f(5)` instantiates `T := i64`, the arm is live, and `g`
            // receives an i64 through a str slot. `T` was opaque, not uninhabited.
            //
            // Tested against the subject rather than against `remaining`, deliberately:
            // `remaining` being empty is the ordinary trailing-`_`-after-an-exhaustive-
            // match case, which is fine and common. It is a test the subject could never
            // satisfy *at all* that is the mistake.
            if let Some(t) = test {
                let live = self.env.solver.t.intersect(subject, t.ty);
                if self.env.solver.is_empty(live)
                    && !self.env.is_error(subject)
                    && !self.env.solver.is_empty(subject)
                {
                    let (expected, found) = (self.show(subject), self.show(t.ty));
                    self.error(
                        arm.pat.span.clone(),
                        TypeErrorKind::Mismatch { expected, found },
                    );
                }
            }
            let bound = match test {
                Some(t) => self.env.solver.t.intersect(remaining, t.ty),
                None => remaining,
            };
            // Never hand an arm an empty binding, reported or not. Poison is the checker's
            // "already dealt with" type; `never` is the one that makes the next check
            // succeed for the wrong reason.
            let bound = if self.env.solver.is_empty(bound) { self.poison() } else { bound };
            if let Some(v) = &scrut_var {
                self.bind(v, bound, scrutinee.span.clone(), DefKind::Refinement);
            }
            self.bind_pattern(module, &arm.pat, bound);
            if let Some(g) = &arm.guard {
                let b = self.env.solver.t.bool();
                self.expr(module, g, Some(b));
            }
            let t = self.expr(module, &arm.body, expected);
            self.locals.pop();
            result = self.union_branches(result, t);

            // Only an exact, unguarded arm removes anything from the fallthrough: `1`
            // is one i64 among many, and a guard can always reject.
            if let Some(test) = test {
                let mut test = test;
                if arm.guard.is_some() {
                    test = test.guarded();
                }
                let covered = test.covered(&mut self.env.solver);
                remaining = self.env.solver.t.diff(remaining, covered);
            }
            if let ast::PatternKind::Literal(lit) = &arm.pat.kind {
                if let (ExprKind::Bool(b), None) = (&lit.kind, &arm.guard) {
                    if *b { saw_true = true } else { saw_false = true }
                }
            }
        }
        if saw_true && saw_false {
            let bool_ty = self.env.solver.t.bool();
            remaining = self.env.solver.t.diff(remaining, bool_ty);
        }

        // Exhaustiveness falls out: whatever is left in `remaining` names exactly the
        // values no arm matched.
        if !self.env.solver.is_empty(remaining) && !self.env.is_error(subject) {
            let missing = self.show(remaining);
            self.error(e.span.clone(), TypeErrorKind::NotExhaustive { missing });
        }
        result
    }

    /// The refinements a boolean condition establishes on bare locals: `(then, else)`
    /// lists of `(name, refined type)` to shadow-bind while checking each branch.
    ///
    /// The conditions that refine are `x is T`, `x == null` / `x != null`, and their
    /// compositions: `not` swaps the branches, `and` refines the then-branch with both
    /// conjuncts (its else concludes nothing), `or` the else-branch with both flipped
    /// disjuncts. Only a bare local narrows — a field or call result has no binding to
    /// shadow. Both refined types come from `narrow::Refined::both`, so neither branch
    /// can receive a `never` binding: a test that always or never matches refines
    /// nothing rather than poisoning a branch (see narrow.rs's module docs for why a
    /// `never` binding is the trap).
    fn cond_refinements(&mut self, cond: &Expr) -> (Vec<(String, TyId)>, Vec<(String, TyId)>) {
        let nothing = (vec![], vec![]);
        match &cond.kind {
            ExprKind::Is { lhs, .. } => {
                let Some(name) = self.scrutinee_var(lhs) else { return nothing };
                let Some(subject) = self.lookup(&name) else { return nothing };
                let Some(tested) = self.result.tested(cond.id) else { return nothing };
                if self.env.is_error(subject) {
                    return nothing;
                }
                let refined = narrow::narrow_is(&mut self.env.solver, subject, tested);
                self.refinement_pair(name, subject, refined, false)
            }
            ExprKind::Binary { op: op @ (BinOp::Eq | BinOp::Ne), lhs, rhs } => {
                let subject_expr = match (&lhs.kind, &rhs.kind) {
                    (ExprKind::Null, _) => rhs,
                    (_, ExprKind::Null) => lhs,
                    _ => return nothing,
                };
                let Some(name) = self.scrutinee_var(subject_expr) else { return nothing };
                let Some(subject) = self.lookup(&name) else { return nothing };
                if self.env.is_error(subject) {
                    return nothing;
                }
                let refined = narrow::narrow_null(&mut self.env.solver, subject);
                self.refinement_pair(name, subject, refined, matches!(op, BinOp::Ne))
            }
            ExprKind::Unary { op: UnOp::Not, rhs } => {
                let (t, e) = self.cond_refinements(rhs);
                (e, t)
            }
            ExprKind::Binary { op: BinOp::And, lhs, rhs } => {
                let (mut tl, _) = self.cond_refinements(lhs);
                let (tr, _) = self.cond_refinements(rhs);
                tl.extend(tr);
                (tl, vec![])
            }
            ExprKind::Binary { op: BinOp::Or, lhs, rhs } => {
                let (_, mut el) = self.cond_refinements(lhs);
                let (_, er) = self.cond_refinements(rhs);
                el.extend(er);
                (vec![], el)
            }
            _ => nothing,
        }
    }

    /// A leaf test's `(then, else)` refinements, with the diff side guarded.
    ///
    /// The match side of a test is an intersection — always positive, always
    /// representable (`any ∧ i64` is `i64`). The complement side is a difference, and
    /// a difference only stays positive when the subject is a union of positives to
    /// subtract from (`A | B ∖ A` is `B`, `T | null ∖ null` is `T`). Subtracting from
    /// `any` leaves a complement (`any ∖ Point`), a type with no runtime
    /// representation of its own — binding it gave the else-branch a repr that was
    /// not `Any`, and lowering then unboxed a value that was never claimed to be
    /// anything. So the diff side binds nothing when the subject is `any`; the branch
    /// simply keeps the unrefined binding. `flip` swaps the branches (`!=` against
    /// `==`) after the guard, so the guard follows the diff wherever it lands.
    fn refinement_pair(
        &mut self,
        name: String,
        subject: TyId,
        refined: narrow::Refined,
        flip: bool,
    ) -> (Vec<(String, TyId)>, Vec<(String, TyId)>) {
        let Some((then_ty, else_ty)) = refined.both() else { return (vec![], vec![]) };
        let top = self.env.solver.t.any();
        let subject_is_top = self.env.solver.is_subtype(top, subject);
        let thens = vec![(name.clone(), then_ty)];
        let elses = if subject_is_top { vec![] } else { vec![(name, else_ty)] };
        if flip {
            (elses, thens)
        } else {
            (thens, elses)
        }
    }

    /// Check `f` with `refs` shadow-bound as refinements in a fresh frame.
    fn with_refinements<R>(
        &mut self,
        refs: &[(String, TyId)],
        span: &Span,
        f: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let frame = refs
            .iter()
            .map(|(n, t)| (n.clone(), *t, span.clone(), DefKind::Refinement))
            .collect();
        self.locals.push(frame);
        let out = f(self);
        self.locals.pop();
        out
    }

    /// The scrutinee's variable name when it is a bare local, so match arms can
    /// re-narrow it in place. A field access or call has no name to rebind.
    fn scrutinee_var(&self, scrutinee: &Expr) -> Option<String> {
        match &scrutinee.kind {
            ExprKind::Path(segs) => match segs.as_slice() {
                [one] if self.lookup(one).is_some() => Some(one.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    /// What an arm tests for, or `None` when it is a plain binding (which admits
    /// everything, and so is not a test at all).
    fn arm_test(&mut self, module: &[String], arm: &ast::MatchArm, subject: TyId) -> Option<narrow::Test> {
        let scope = self.type_scope(module);
        match &arm.pat.kind {
            ast::PatternKind::Wildcard | ast::PatternKind::Bind(_) => {
                Some(narrow::Test::exact(subject))
            }
            ast::PatternKind::Is(spec) => {
                let t = self.env.resolve(&scope, spec);
                // A structural test on an opaque-holding subject is introspection, and
                // its narrowing would intersect the structural atom into the binding —
                // past the point where the flow gate could still see whose contents it
                // was. See `ExprKind::Is`.
                self.member_gate(
                    module,
                    &arm.pat.span,
                    subject,
                    t,
                    "it can be tested against a structural view",
                );
                self.result.set_tested(arm.pat.id, t);
                Some(narrow::Test::exact(t))
            }
            ast::PatternKind::Literal(lit) => {
                let t = self.expr(module, lit, None);
                // An atom and `null` are singletons; an integer literal is one i64
                // among many, so it covers nothing.
                Some(match &lit.kind {
                    ExprKind::Atom(_) | ExprKind::Null => narrow::Test::exact(t),
                    _ => narrow::Test::inexact(t),
                })
            }
            // A named record pattern selects its member and narrows to it, so
            // `Circle { r }` reads `r` as an `i64`. It covers that member only when
            // every field pattern is irrefutable -- `Circle { r: 0 }` matches one
            // Circle among many, so it covers nothing and needs a fallthrough.
            ast::PatternKind::Record { path: Some(p), fields, .. } => {
                let spec = ast::TypeSpec {
                    kind: ast::TypeSpecKind::Named { path: p.clone(), args: vec![] },
                    span: arm.pat.span.clone(),
                };
                let t = self.env.resolve(&scope, &spec);
                self.result.set_tested(arm.pat.id, t);
                let exact = fields.iter().all(Self::field_irrefutable);
                Some(if exact { narrow::Test::exact(t) } else { narrow::Test::inexact(t) })
            }
            _ => None,
        }
    }

    /// Whether a field pattern can never reject — a shorthand or a bind always
    /// matches; a nested literal or `is` can fail.
    fn field_irrefutable(f: &ast::FieldPat) -> bool {
        f.pat.as_ref().is_none_or(Self::pat_irrefutable)
    }

    /// Whether a pattern matches every value of the type it is applied to. A literal or
    /// an `is` can reject, so anything containing one is refutable; a bind, a wildcard and
    /// aggregates built only out of those cannot.
    ///
    /// This decides *exactness*, not well-typedness: a refutable arm still binds and still
    /// checks, it just subtracts nothing from the fallthrough, so `Circle { r: 0 }` leaves
    /// the other Circles to be handled.
    fn pat_irrefutable(p: &ast::Pattern) -> bool {
        match &p.kind {
            ast::PatternKind::Bind(_) | ast::PatternKind::Wildcard => true,
            ast::PatternKind::Record { fields, .. } => fields.iter().all(Self::field_irrefutable),
            ast::PatternKind::Tuple(ps) => ps.iter().all(Self::pat_irrefutable),
            _ => false,
        }
    }

    /// A call. The callee decides which of four machines runs, and the order they are
    /// tried in is the language's shadowing rule made concrete: a local shadows a module
    /// function, which shadows protocol dispatch.
    ///
    /// - a non-path callee (a lambda, a parenthesised call, a field holding a function)
    ///   is `apply`: callable iff its type is an arrow;
    /// - a single-segment path bound as a local is `apply` too — a first-class function
    ///   value being called, not a name in the fn table;
    /// - a resolvable function name is `direct_call`, which knows about generics,
    ///   `where` bounds and declared `throws`;
    /// - anything left goes to protocol dispatch on the argument types.
    ///
    /// Arguments are checked in whichever branch is taken, so each is visited exactly
    /// once — except in a generic direct call, which checks them twice by design and
    /// relies on `check_all`'s deduplication to keep that from doubling diagnostics.
    ///
    /// The `x.f(..)` probe at the top exists because Neon has no method-call syntax. If
    /// `f` is not a field of `x`, the user meant a method, and saying so beats letting it
    /// fail as a missing field.
    fn call(
        &mut self,
        module: &[String],
        e: &Expr,
        callee: &Expr,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> TyId {
        // `x.f(..)` is either a call of a field that holds a function, or method-call
        // syntax -- which Neon does not have. Tell them apart by whether `f` is a
        // field: if not, suggest the free-function or pipe form rather than letting
        // it fail as a plain missing field.
        if let ExprKind::Field { base, name } = &callee.kind {
            let base_ty = self.expr(module, base, None);
            let label = self.env.solver.t.name(name);
            let field = narrow::project_field(&mut self.env.solver, base_ty, label);
            if field.ty().is_none() && !self.env.is_error(base_ty) {
                let on = self.show(base_ty);
                self.error(callee.span.clone(), TypeErrorKind::DotCall { method: name.clone(), on });
                return self.poison();
            }
        }

        let ExprKind::Path(p) = &callee.kind else {
            // Any other expression producing a value: a lambda, a field holding a
            // function, a parenthesised call. It is callable iff its type is an arrow.
            let t = self.expr(module, callee, None);
            return self.apply(module, e, "this expression", t, args);
        };

        // Lexical first: a local shadows everything. A local of arrow type is a
        // first-class value being called, not a name to look up in the fn table.
        if let [one] = p.as_slice() {
            if let Some(t) = self.lookup(one) {
                self.result.set_ty(callee.id, t);
                // Keyed on the callee, not the call: the name the user can click is the
                // callee's span, and `set_call` already covers the call itself.
                if let Some((span, kind)) = self.binding_of(one) {
                    let site = DefSite { module: module.to_vec(), span, kind };
                    self.result.set_def(callee.id, site);
                }
                return self.apply(module, e, one, t, args);
            }
        }

        // Then a module fn, which shadows protocols.
        if let Some(sig) = self.env.fn_named(module, p).cloned() {
            self.result.set_ty(callee.id, sig.ty);
            let site = DefSite {
                module: sig.module.clone(),
                span: sig.span.clone(),
                kind: DefKind::Fn,
            };
            self.result.set_def(callee.id, site);
            return self.direct_call(module, e, &sig, generics, args, expected);
        }

        // A function that exists but is fenced off says so, rather than falling through
        // to protocol dispatch and reporting a missing method.
        if let Some(owner) = self.env.hidden_by_internal(module, p) {
            self.error(
                callee.span.clone(),
                TypeErrorKind::Internal { name: p.join("::"), owner },
            );
            return self.poison();
        }

        let arg_tys: Vec<TyId> = args
            .iter()
            .map(|a| {
                // An argument is a value position even when the callee is a protocol
                // method whose parameter type is not yet known, so a bare `if` is
                // reported here rather than as the `()` it would otherwise dispatch on.
                self.reject_bare_if(a);
                self.expr(module, a, None)
            })
            .collect();
        let (name, qualified) = match p.split_last() {
            // A bare name may have been imported as a specific protocol's method.
            Some((last, [])) => (last.clone(), self.env.imported_method(module, last)),
            Some((last, rest)) => (last.clone(), self.env.lookup_protocol(module, rest)),
            None => return self.poison(),
        };

        match dispatch::resolve(self.env, &name, qualified, &arg_tys, expected) {
            Ok(s) => {
                self.dispatch_gate(module, &e.span, &s, &arg_tys);
                if let dispatch::Resolution::Bound { param, protocol } = &s.resolution {
                    let ok = self.bounds.iter().any(|(n, p)| {
                        n == param && self.env.protocol_extends(*p, *protocol)
                    });
                    if !ok {
                        let pname = self.env.protocols()[protocol.0].name.clone();
                        self.error(
                            e.span.clone(),
                            TypeErrorKind::UnsatisfiedBound { ty: param.clone(), protocol: pname },
                        );
                    }
                }
                self.result.set_call(e.id, s.resolution.clone());
                self.note_throw(module, e.span.clone(), s.throws, true);
                s.ret
            }
            Err(err) => {
                self.dispatch_error(e.span.clone(), err);
                self.poison()
            }
        }
    }

    /// A call of a named function whose signature is already known.
    ///
    /// The non-generic case is simply flowing each parameter type down as the argument's
    /// expected type, which is what makes `map(xs, (x) => x + 1)` give `x` a type.
    ///
    /// The generic case solves the type parameters first (`solve_generics`) and then
    /// re-checks every argument under the substitution. Checking twice is the price of
    /// having an expected type available for arguments that need one, and it is why
    /// `check_all` deduplicates the finished error list — the alternative was threading a
    /// "probing, stay quiet" mode through every expression form.
    ///
    /// An arity mismatch is reported and then *continues*: the surplus or missing
    /// arguments are still checked with no expected type, so a wrong count does not
    /// suppress the mistakes inside the arguments themselves.
    fn direct_call(
        &mut self,
        module: &[String],
        e: &Expr,
        sig: &super::env::FnSig,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> TyId {
        if sig.params.len() != args.len() {
            self.error(
                e.span.clone(),
                TypeErrorKind::Arity {
                    name: sig.name.clone(),
                    expected: sig.params.len(),
                    found: args.len(),
                },
            );
        }

        // A non-generic fn: flow each parameter type into its argument as the
        // expected type, so a lambda argument infers its parameters.
        if sig.generics.is_empty() {
            for (a, (_, want)) in args.iter().zip(&sig.params) {
                self.expr(module, a, Some(*want));
            }
            for a in args.iter().skip(sig.params.len()) {
                self.expr(module, a, None);
            }
            self.note_throw(module, e.span.clone(), sig.throws, true);
            return sig.ret;
        }

        // A generic fn: solve its type parameters, then check under the solution.
        let subst = self.solve_generics(module, sig, generics, args, expected);
        // Hand the solution to lowering, which needs the *types* the parameters were bound
        // to in order to lay the instance out.
        let solved: Vec<(String, TyId)> = sig
            .generics
            .iter()
            .filter_map(|g| {
                let n = self.env.solver.t.name(g);
                subst.get(&n).map(|&t| (g.clone(), t))
            })
            .collect();
        self.result.set_generics(e.id, solved);
        // Discharge each `where T: P`: the type T was bound to must satisfy P here.
        for (param, proto_path) in &sig.wheres {
            let pn = self.env.solver.t.name(param);
            let Some(&concrete) = subst.get(&pn) else { continue };
            if self.env.is_error(concrete) {
                continue;
            }
            let Some(pid) = self.env.lookup_protocol(module, proto_path) else { continue };
            // A marker is answered from structure, so it is checked *here* rather than
            // through `type_satisfies`: this is the only place that knows the enclosing
            // signature's own bounds, and they are what make a still-generic argument
            // satisfiable. `sort[T](xs: List[T]) where T: Ord` calling `max(xs, xs)` passes
            // `List[T]`, which is ordered exactly because `T` is bound here -- ask without
            // that context and it is not.
            let satisfied = if self.env.protocol(pid).is_marker {
                self.is_ordered(concrete)
            } else if super::generic::is_var(&self.env.solver.t, concrete) {
                // A protocol bound on a still-abstract argument is the caller's own bound
                // to discharge, checked where that caller is called.
                continue;
            } else {
                self.env.type_satisfies(concrete, pid)
            };
            if !satisfied {
                let (ty, name) = (self.show(concrete), proto_path.join("::"));
                let kind = if self.env.protocol(pid).is_marker {
                    TypeErrorKind::UnsatisfiedMarker { ty, marker: name }
                } else {
                    TypeErrorKind::UnsatisfiedBound { ty, protocol: name }
                };
                self.error(e.span.clone(), kind);
            } else if !self.env.protocol(pid).is_marker {
                // Satisfied — but through which impls? Inside the callee the bound
                // resolves as `Resolution::Bound` and runs whatever impl covers the
                // concrete type at run time, so a bound discharged through an impl for
                // a structural type is the dispatch back door one call further out:
                // `where T: Peek` with `impl Peek for {code: i64}` accepts a `Secret`
                // and hands it to that impl. Every covering impl target is a view the
                // value flows into; gate each.
                self.bound_gate(module, &e.span, concrete, pid);
            }
        }
        for (a, (_, template)) in args.iter().zip(&sig.params) {
            let want = self.env.solver.t.substitute(*template, &subst);
            self.expr(module, a, Some(want));
        }
        for a in args.iter().skip(sig.params.len()) {
            self.expr(module, a, None);
        }
        let throws = self.env.solver.t.substitute(sig.throws, &subst);
        self.note_throw(module, e.span.clone(), throws, true);
        self.env.solver.t.substitute(sig.ret, &subst)
    }

    /// The substitution for a generic call's type parameters: a turbofish if
    /// present, else inferred from the argument types and the expected result.
    fn solve_generics(
        &mut self,
        module: &[String],
        sig: &super::env::FnSig,
        generics: &[ast::TypeSpec],
        args: &[Expr],
        expected: Option<TyId>,
    ) -> std::collections::HashMap<super::types::NameId, TyId> {
        use std::collections::{HashMap, HashSet};
        let mut subst: HashMap<_, TyId> = HashMap::new();
        let var_names: HashSet<_> =
            sig.generics.iter().map(|g| self.env.solver.t.name(g)).collect();

        if !generics.is_empty() {
            let scope = self.type_scope(module);
            for (g, spec) in sig.generics.iter().zip(generics) {
                let ty = self.env.resolve(&scope, spec);
                let n = self.env.solver.t.name(g);
                subst.insert(n, ty);
            }
            return subst;
        }

        // Top-down before bottom-up: the expected result sets a variable first, and
        // `infer` is first-wins, so the arguments then conform to it rather than
        // widening it. That is what lets `-> List[i64|str] { push(xs, "s") }` widen
        // on request while a bare `push(xs, "s")` pins `T := i64` and rejects the str.
        if let Some(exp) = expected {
            super::generic::infer(&mut self.env.solver.t, sig.ret, exp, &var_names, &mut subst);
        }
        let arg_tys: Vec<TyId> = args.iter().map(|a| self.expr(module, a, None)).collect();
        for ((_, template), &aty) in sig.params.iter().zip(&arg_tys) {
            super::generic::infer(&mut self.env.solver.t, *template, aty, &var_names, &mut subst);
        }
        subst
    }

    /// Call a value. `callee_ty` must be an arrow; `what` names it for diagnostics.
    fn apply(&mut self, module: &[String], e: &Expr, what: &str, callee_ty: TyId, args: &[Expr]) -> TyId {
        if self.env.is_error(callee_ty) {
            for a in args {
                self.expr(module, a, None);
            }
            return self.poison();
        }
        let Some(arrow) = self.env.solver.t.as_arrow(callee_ty) else {
            for a in args {
                self.expr(module, a, None);
            }
            let ty = self.show(callee_ty);
            self.error(e.span.clone(), TypeErrorKind::NotCallable { what: what.to_string(), ty });
            return self.poison();
        };
        if arrow.params.len() != args.len() {
            self.error(
                e.span.clone(),
                TypeErrorKind::Arity {
                    name: what.to_string(),
                    expected: arrow.params.len(),
                    found: args.len(),
                },
            );
        }
        for (a, want) in args.iter().zip(&arrow.params) {
            self.expr(module, a, Some(*want));
        }
        for a in args.iter().skip(arrow.params.len()) {
            self.expr(module, a, None);
        }
        // A call through a value throws what its arrow says — same rule as a direct
        // call: bare outside a `try`, it is an error; inside one, it lands in the sink.
        self.note_throw(module, e.span.clone(), arrow.throws, true);
        arrow.ret
    }

    /// A `DispatchError` as a user-facing diagnostic. `NoImpl`'s payload is a `TyId` and
    /// so has to be printed here: it is the part of the receiver with no impl, which for
    /// a union is the variants that were missed rather than the whole union, leaving the
    /// reader nothing to work out.
    fn dispatch_error(&mut self, span: Span, err: DispatchError) {
        let kind = match err {
            DispatchError::UnknownMethod(n) => TypeErrorKind::UnknownName(n),
            DispatchError::Ambiguous { method, protocols } => {
                TypeErrorKind::AmbiguousCall { method, protocols }
            }
            DispatchError::NoImpl { protocol, method, uncovered } => {
                let uncovered = self.show(uncovered);
                TypeErrorKind::NoImpl { protocol, method, uncovered }
            }
            DispatchError::NoReceiver(n) => TypeErrorKind::NoReceiver(n),
        };
        self.error(span, kind);
    }
}

