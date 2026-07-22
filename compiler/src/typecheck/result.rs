//! The checker's output, and the contract between typechecking and lowering.
//!
//! Everything here is written once by `check.rs` and read many times by `ir/`. The rule
//! the module exists to enforce is one-directional: **lowering asks, it never derives.**
//! Each map records a decision the checker had the information to make and lowering does
//! not — a solved generic argument, the union a `try` can catch, the declared type of a
//! binding whose initialiser is narrower. Where such a map is missing, lowering has
//! historically guessed, and guessing about a type ends in erasure.
//!
//! Every map is keyed by `ExprId`, which is unique across the whole compilation (see
//! `ast::ids`), so one `TypecheckResult` covers every module and nothing has to be
//! namespaced by file.

use super::dispatch::Resolution;
use super::types::TyId;
use crate::ast::ExprId;
use crate::lexer::Span;
use std::collections::HashMap;

/// What sort of thing a name turned out to name. Carried alongside the span so a
/// consumer can label a jump without re-deriving the answer from the AST.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefKind {
    /// A `let`, or a binding introduced by `for`, `case`, or a `try` handler.
    Local,
    /// A flow refinement shadowing an existing binding — a match arm's narrowed
    /// scrutinee, or an `is`/null-test's refined subject in an `if`/`while` branch.
    /// Reads see it; an assignment sees through it to the declared binding beneath
    /// (and dissolves it — after `x = e` the refinement no longer holds).
    Refinement,
    /// A function parameter, including a lambda's.
    Param,
    /// A top-level or module-level `fn`.
    Fn,
    /// A top-level or module-level `const`.
    Const,
}

/// Where a name was defined: enough to open the right file at the right range.
///
/// `module` is here because a span alone is ambiguous. Spans are byte offsets into
/// *some* source, and a compilation covers many — the user's module plus every stdlib
/// file, all checked together so that one `TypecheckResult` spans them all. Recording
/// which module a span belongs to is what makes the offset resolvable to a file; without
/// it, a jump into `std::io` would land at the same byte offset in whatever file the
/// editor happened to have open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefSite {
    pub module: Vec<String>,
    pub span: Span,
    pub kind: DefKind,
}

/// What the checker learned, keyed by expression.
///
/// `expr_types` is the keystone. The previous implementation kept only the
/// resolutions and **threw every expression type away**, so IR lowering had to
/// re-derive them — which is why `infer.rs` existed. It could not always succeed,
/// so it fell back to `Erased`; that leaked into `NeonValue` boxing, which invented
/// vtables, which produced `*_Any` collections with 24-byte slots that `push` read
/// as 8 — an ASan stack-buffer-overflow on every `list::new()`.
///
/// One discarded hashmap, four subsystems of consequences. Nothing downstream
/// re-derives or re-resolves anything here.
#[derive(Debug, Default)]
pub struct TypecheckResult {
    expr_types: HashMap<ExprId, TyId>,
    resolved_calls: HashMap<ExprId, Resolution>,
    /// The `to_string` dispatch of a string-interpolation hole, keyed by the hole
    /// expression's id — in its own table because the hole may itself be a dispatched
    /// call whose resolution lives in `resolved_calls` under the same id. One id, two
    /// resolutions, two tables: storing both in one overwrote the call's, and lowering
    /// then had to suppress dispatch for the whole subtree, which mislowered
    /// `"#{area(q)}"` into `<todo: path-as-value>` on a string constant.
    interp_calls: HashMap<ExprId, Resolution>,
    /// Where each name-shaped expression's referent was defined.
    ///
    /// Nothing in the compiler reads this: lowering resolves names through the same
    /// scope walk the checker did, and does not need the answer handed to it. It exists
    /// for the editor, which cannot redo that walk — the checker is the only pass that
    /// ever holds "this `x` is *that* `x`", and it held it for the duration of one
    /// function call before dropping it on the floor. Every jump-to-definition,
    /// find-references and rename is this map read forwards or backwards.
    ///
    /// Recorded at the single point where a path resolves (`check.rs::path`), so a name
    /// the checker could not resolve is simply absent rather than wrong.
    resolved_names: HashMap<ExprId, DefSite>,
    /// A lambda's inferred signature, as an arrow. Currently redundant: `check.rs`
    /// records the same arrow into `expr_types` for the lambda expression, and lowering
    /// reads it from there. Nothing reads this map.
    resolved_lambdas: HashMap<ExprId, TyId>,
    /// The error type a `try` can catch — the union of what its body throws. Recorded so
    /// lowering gives the handler a *concrete* error parameter; without it the error
    /// channel falls back to `any`, which is erasure leaking in by the back door.
    caught_types: HashMap<ExprId, TyId>,
    /// A generic call's solved type arguments, by parameter name. Recorded because
    /// lowering otherwise re-derives them from the turbofish *syntax*, which carries only
    /// a type's head — enough to mangle a name, not enough to lay one out.
    generic_args: HashMap<ExprId, Vec<(String, TyId)>>,
    /// A `let`'s declared type, keyed on its initialiser. The annotation is the binding's
    /// type -- `let x: i64 | str = 1` binds the union -- but lowering sees only the
    /// initialiser, whose type is the narrow one. Without this the binding was laid out at
    /// the *variant's* repr: `let n: P | :none = :none` became a bare tag, and any later
    /// use expecting the union read the wrong layout.
    declared_types: HashMap<ExprId, TyId>,
    /// The type an `is` asks about, resolved. Keyed by the `is` *expression*, or by the
    /// `Pattern` for `case is T` / `case Circle { .. }`.
    ///
    /// Recorded because the alternative is lowering reading the written path — and a path
    /// is a head name. `a is List[str]` and `a is List[i64]` write the same head, so the
    /// runtime test compiled to the same comparison for both, `List[i64] is List[str]` was
    /// true, and the `as` that follows such a guard reinterpreted i64s as string headers.
    /// The resolved type carries the arguments the path only spells, which lets the
    /// backend derive the tag with the *same* function that stamped it into the box.
    tested_types: HashMap<ExprId, TyId>,
}

impl TypecheckResult {
    /// `None` means the checker never visited this expression — which, after a clean
    /// check, means it is not an expression whose value anything can observe. Lowering
    /// treating `None` as "infer it myself" is exactly the failure this module exists to
    /// prevent; treat it as a checker bug instead.
    pub fn ty(&self, e: ExprId) -> Option<TyId> {
        self.expr_types.get(&e).copied()
    }

    /// Every expression's type. The IR walks these to assign a representation to each,
    /// which is why the whole map has to survive rather than just the entries a lowering
    /// walk happens to ask for. Iteration order is a `HashMap`'s, so nothing built from
    /// this may depend on order.
    pub fn types(&self) -> impl Iterator<Item = (ExprId, TyId)> + '_ {
        self.expr_types.iter().map(|(&e, &t)| (e, t))
    }

    /// Which function or impl method a call selected. Keyed on the *call* expression, so
    /// the same callee name resolved differently at two sites stays distinct — that is
    /// the point of recording it, since lowering cannot redo protocol dispatch without
    /// the argument types the checker had.
    pub fn call(&self, e: ExprId) -> Option<&Resolution> {
        self.resolved_calls.get(&e)
    }

    /// The `to_string` resolution of an interpolation hole. Distinct from `call` on
    /// the same id — see `interp_calls`.
    pub fn interp_call(&self, e: ExprId) -> Option<&Resolution> {
        self.interp_calls.get(&e)
    }

    pub fn set_interp_call(&mut self, e: ExprId, r: Resolution) {
        self.interp_calls.insert(e, r);
    }

    /// Where the name this expression writes was defined, if it resolved to one.
    ///
    /// `None` covers two cases an editor must not confuse: the expression is not a name
    /// at all, or it is a name that did not resolve. Both mean "no jump available", which
    /// is the same answer, so neither is worth distinguishing here.
    pub fn def(&self, e: ExprId) -> Option<&DefSite> {
        self.resolved_names.get(&e)
    }

    /// Every resolved name. An editor builds its reverse index from this — find-references
    /// is "which ids map to the site this one maps to", which needs the whole map, not a
    /// lookup. Iteration order is a `HashMap`'s; sort before rendering.
    pub fn defs(&self) -> impl Iterator<Item = (ExprId, &DefSite)> + '_ {
        self.resolved_names.iter().map(|(&e, d)| (e, d))
    }

    /// Records what a path resolved to. See `resolved_names`.
    pub(super) fn set_def(&mut self, e: ExprId, d: DefSite) {
        self.resolved_names.insert(e, d);
    }

    /// See `resolved_lambdas`: nothing calls this, and the same arrow is available from
    /// `ty` for the lambda's own expression id.
    pub fn lambda(&self, e: ExprId) -> Option<TyId> {
        self.resolved_lambdas.get(&e).copied()
    }

    /// A generic call's solved type arguments.
    pub fn generics(&self, e: ExprId) -> Option<&[(String, TyId)]> {
        self.generic_args.get(&e).map(Vec::as_slice)
    }

    /// The declared type of the `let` whose initialiser this is. Note the key is the
    /// *initialiser* expression, not the binding — the binding has no `ExprId` — so
    /// lowering asks this of the value it is about to store.
    pub fn declared(&self, e: ExprId) -> Option<TyId> {
        self.declared_types.get(&e).copied()
    }

    /// Records the *annotation*, not the initialiser's own type — recording the latter
    /// would make this map a no-op. `pub` rather than `pub(super)` only by inconsistency;
    /// the sole caller is `check.rs`.
    pub fn set_declared(&mut self, e: ExprId, t: TyId) {
        self.declared_types.insert(e, t);
    }

    /// The type an `is` test names, resolved. See `tested_types`.
    pub fn tested(&self, e: ExprId) -> Option<TyId> {
        self.tested_types.get(&e).copied()
    }

    /// Records the type an `is` test (expression or pattern) resolved its path to.
    pub(super) fn set_tested(&mut self, e: ExprId, t: TyId) {
        self.tested_types.insert(e, t);
    }

    /// The error type a `try` expression's handler receives.
    pub fn caught(&self, e: ExprId) -> Option<TyId> {
        self.caught_types.get(&e).copied()
    }

    /// How many expressions were typed. Only the tests use this, as a coarse assertion
    /// that the checker recorded types at all rather than silently recording none — the
    /// regression the module doc describes was invisible precisely because an empty
    /// result still compiled.
    pub fn len(&self) -> usize {
        self.expr_types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.expr_types.is_empty()
    }

    /// The blanket write is at the end of `check.rs`'s `expr`, so every expression that
    /// goes through `expr` is recorded whatever path produced its type. The other call
    /// sites exist for the few expressions the checker types *without* going through
    /// `expr` — a record-literal field value, which is inferred directly so a mismatch can
    /// name the field, and a call's callee, which is looked up rather than evaluated. Each
    /// is a subexpression lowering will still ask about, so skipping them would leave a
    /// hole in `expr_types`.
    pub(super) fn set_ty(&mut self, e: ExprId, t: TyId) {
        self.expr_types.insert(e, t);
    }

    /// Keyed on the call expression, never on the callee: the same callee resolves
    /// differently at different sites, and the call is what lowering is standing on when
    /// it needs the answer.
    pub(super) fn set_call(&mut self, e: ExprId, r: Resolution) {
        self.resolved_calls.insert(e, r);
    }

    /// Only the parameters the solver pinned down. A generic left unsolved is absent
    /// rather than recorded as `any`, so a missing entry is a real gap and not a lie
    /// lowering would go on to lay out.
    pub(super) fn set_generics(&mut self, e: ExprId, args: Vec<(String, TyId)>) {
        self.generic_args.insert(e, args);
    }

    pub(super) fn set_caught(&mut self, e: ExprId, t: TyId) {
        self.caught_types.insert(e, t);
    }

    /// Called for every lambda, but nothing reads the map back — see `resolved_lambdas`.
    #[allow(dead_code)]
    pub(super) fn set_lambda(&mut self, e: ExprId, t: TyId) {
        self.resolved_lambdas.insert(e, t);
    }
}
