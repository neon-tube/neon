//! The syntax tree.
//!
//! Spans are carried on every node a diagnostic can point at.
//!
//! The tree is what the parser produced and nothing more: it is deliberately *not*
//! resolved, normalised or desugared. A path stays a list of segments rather than a
//! binding, a literal keeps the text it was written with, and sugar keeps its own node
//! rather than becoming its expansion. That is what allows one tree to serve both the
//! checker and the formatter — a formatter fed a desugared tree cannot print the program
//! back, and a tree that normalises anything has already made a decision the formatter is
//! then obliged to reproduce.
//!
//! Two identities ride alongside the tree, and they answer different questions.
//! [`ExprId`] is *which expression* — stable, assigned in pre-order by [`number_exprs`],
//! and what the checker keys types on. A [`Span`] is *where the text is* — not unique
//! (two nodes can share one) and legitimately changed by formatting, which is why
//! [`strip_spans`] exists to take it out of a tree comparison.

mod ids;
mod spans;
pub mod visit;

#[cfg(test)]
mod tests;

pub use ids::{number_exprs, number_exprs_from};
pub use spans::strip_spans;

use crate::lexer::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    pub decls: Vec<Decl>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Decl {
    pub kind: DeclKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum DeclKind {
    Fn(FnDecl),
    Record(RecordDecl),
    Protocol(ProtocolDecl),
    Impl(ImplDecl),
    /// `type A = B`. Non-recursive: a recursive plain alias is an error.
    TypeAlias(AliasDecl),
    /// `mu type A = ...`. The binder asserts recursion.
    MuType(AliasDecl),
    /// `newtype A = B`. Nominal wrapper; may not be recursive.
    Newtype(AliasDecl),
    Use(UseDecl),
    Mod(ModDecl),
    Const(ConstDecl),
    TestBlock(TestBlock),
    /// Recovery produces this so one bad declaration does not discard the file.
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FnDecl {
    pub name: String,
    pub generics: Vec<String>,
    pub params: Vec<Param>,
    /// `throws E`, written before `->`.
    pub throws: Option<TypeSpec>,
    pub ret: Option<TypeSpec>,
    pub wheres: Vec<WhereClause>,
    /// `None` for a protocol's required method, which has no body.
    pub body: Option<Block>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Annotation {
    pub name: String,
    pub arg: Option<String>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct WhereClause {
    pub param: String,
    pub bound: TypeSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub ty: TypeSpec,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecordDecl {
    pub name: String,
    pub generics: Vec<String>,
    /// The representation is not part of the interface: no structural views,
    /// construction, or probes outside the declaring module's subtree.
    pub opaque: bool,
    /// `opaque`, plus a trust boundary: outsiders may not assert the type out of an
    /// erased value (`as!`), and may not order values of it. `sealed` implies
    /// `opaque`; the explicit pair `sealed opaque` is permitted. Capability types —
    /// `File`, `Resource` — are sealed; data types with hidden layout are merely
    /// opaque.
    pub sealed: bool,
    pub fields: Vec<Field>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub ty: TypeSpec,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProtocolDecl {
    pub name: String,
    /// `protocol Name for T { ... }`
    pub subject: String,
    /// `protocol Name for C[_] { ... }` — the subject is a type constructor of
    /// this arity. 0 for a plain type.
    pub subject_arity: usize,
    /// `protocol Ord for T where T: Eq` — protocols the subject must also satisfy.
    /// Implementing this protocol obliges the type to implement each of these.
    pub wheres: Vec<WhereClause>,
    pub methods: Vec<FnDecl>,
    pub annotations: Vec<Annotation>,
    /// `marker Ord` rather than `protocol Ord for T { .. }`: no methods, no impls,
    /// satisfied by a rule the compiler knows. Shares this node because a marker *is*
    /// a protocol with an empty method set -- only satisfaction differs.
    pub is_marker: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImplDecl {
    /// `orphan impl P for T` — the author saying out loud that they own neither
    /// side. Legal only in the root application, and only to fill a gap.
    pub orphan: bool,
    pub protocol: Vec<String>,
    pub generics: Vec<String>,
    pub target: TypeSpec,
    pub methods: Vec<FnDecl>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AliasDecl {
    pub name: String,
    pub generics: Vec<String>,
    pub value: TypeSpec,
}

#[derive(Debug, Clone, PartialEq)]
pub struct UseDecl {
    pub tree: UseTree,
    pub span: Span,
}

/// `use` mirrors Rust's tree: a leaf path with an optional rename, a glob, or a
/// braced group that shares a prefix. `use x::{y as z, sub::*}` is one declaration.
#[derive(Debug, Clone, PartialEq)]
pub enum UseTree {
    /// `a::b::c`, or `a::b as name`. Without an alias the bound name is the last
    /// segment. The last segment may name a fn, a type, or a protocol method.
    Leaf { path: Vec<String>, alias: Option<String> },
    /// `prefix::*` — every name under `prefix` becomes visible.
    Glob { prefix: Vec<String> },
    /// `prefix::{ children }`.
    Group { prefix: Vec<String>, children: Vec<UseTree> },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModDecl {
    pub name: String,
    pub internal: bool,
    pub decls: Vec<Decl>,
    pub annotations: Vec<Annotation>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ConstDecl {
    pub name: String,
    pub ty: Option<TypeSpec>,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TestBlock {
    pub kind: TestKind,
    pub name: String,
    pub body: Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestKind {
    Test,
    Bench,
}

// ---- types ----

#[derive(Debug, Clone, PartialEq)]
pub struct TypeSpec {
    pub kind: TypeSpecKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeSpecKind {
    /// `i64`, `List[T]`, `std::io::Reader`
    Named { path: Vec<String>, args: Vec<TypeSpec> },
    /// `:ok` as a type — the singleton inhabited by that atom.
    Atom(String),
    Null,
    /// The one legitimate erasure boundary.
    Any,
    /// `{ name: str, age: i64 }`
    Struct(Vec<Field>),
    /// `A | B`
    Union(Vec<TypeSpec>),
    /// `A & B`
    Intersect(Vec<TypeSpec>),
    /// `!A`
    Negate(Box<TypeSpec>),
    /// `(A, B) throws E -> C`. An absent `throws` is `never`.
    Fn { params: Vec<TypeSpec>, throws: Option<Box<TypeSpec>>, ret: Box<TypeSpec> },
    /// `(A, B)`
    Tuple(Vec<TypeSpec>),
    Error,
}

// ---- statements ----

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    /// The block's value: a trailing expression with no semicolon.
    pub tail: Option<Box<Expr>>,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub kind: StmtKind,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StmtKind {
    /// Bindings rebind; there is no `mut`.
    Let { pat: Pattern, ty: Option<TypeSpec>, value: Expr },
    /// `x = e`. A single name, never a path: only a binding can be rebound, so
    /// `a::b = e` is meaningless and the parser rejects it rather than letting
    /// the tree carry something no later pass could act on.
    Assign { name: String, value: Expr },
    Expr(Expr),
    Error,
}

// ---- expressions ----

/// Stable per-expression identity, assigned by a pre-order pass after parsing.
///
/// The checker records a type for every expression keyed on this. Spans are not
/// usable for the job: two expressions can share a span, and a span is a fact
/// about source position that formatting is allowed to change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ExprId(pub u32);

impl ExprId {
    /// What the parser builds. `number_exprs` replaces it.
    pub const UNSET: ExprId = ExprId(u32::MAX);
}

#[derive(Debug, Clone, PartialEq)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
    pub id: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ExprKind {
    /// The magnitude only; `-` is a unary operator applied to it. Keeping the
    /// sign out of the literal is what makes `-9223372036854775808` expressible.
    Int(u64),
    /// Kept as written, not parsed: `1.0`, `1.00` and `1e0` are the same value
    /// but not the same text, and the formatter must not silently rewrite one
    /// into another.
    Float(String),
    Str(Vec<StrPart>),
    Rune(char),
    Atom(String),
    Bool(bool),
    Null,
    Path(Vec<String>),
    Unary { op: UnOp, rhs: Box<Expr> },
    Binary { op: BinOp, lhs: Box<Expr>, rhs: Box<Expr> },
    /// `f(a, b)`; `f[T](a)` carries turbofish args.
    Call { callee: Box<Expr>, generics: Vec<TypeSpec>, args: Vec<Expr> },
    /// `xs[i]` — traps on a bad index rather than throwing.
    Index { base: Box<Expr>, index: Box<Expr> },
    /// `p.field`
    Field { base: Box<Expr>, name: String },
    /// `[1, 2, ..rest]`
    List(Vec<Elem>),
    /// `Point { x: 1, ..base }`, or `{ x: 1 }` with `path: None` — the
    /// anonymous record that optional parameters arrive in.
    RecordLit { path: Option<Vec<String>>, fields: Vec<FieldInit>, spread: Option<Box<Expr>> },
    /// `(a, b)`
    Tuple(Vec<Expr>),
    /// `(x) => e`, `(x: i64) => e`
    Lambda { params: Vec<LambdaParam>, body: Box<Expr> },
    /// `else` is required when the value is consumed; the parser records its
    /// absence rather than substituting null.
    If { cond: Box<Expr>, then: Block, else_: Option<Box<Expr>> },
    Match { scrutinee: Box<Expr>, arms: Vec<MatchArm> },
    Block(Block),
    Loop { body: Block },
    While { cond: Box<Expr>, body: Block },
    For { pat: Pattern, iter: Box<Expr>, body: Block },
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
    Throw(Box<Expr>),
    /// `try e`, `try? e`, `try! e`, and `try e catch (x) { .. }`. All forms
    /// accept a block.
    Try { form: TryForm, body: Box<Expr>, catch: Option<CatchArm> },
    /// `x is T`
    Is { lhs: Box<Expr>, ty: TypeSpec },
    /// `x as T`, `x as? T`, `x as! T`
    As { form: CastForm, lhs: Box<Expr>, ty: TypeSpec },
    /// `assert(..)`, `assert_eq(..)` — intrinsics, so failures can report the
    /// actual values and a span.
    Assert { kind: AssertKind, args: Vec<Expr> },
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TryForm {
    /// Propagate to the caller.
    Propagate,
    /// Soften to `T | null`.
    Soften,
    /// Assert: a failure panics.
    Assert,
}

/// The cast triad, the `try` triad's twin: a cast that cannot fail is spelled bare,
/// one that can must say what a mismatch does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CastForm {
    /// `as` — infallible coercions only: subsumption, widening, newtype wrap/unwrap.
    Plain,
    /// `as?` — test; yields `T | null`.
    Soften,
    /// `as!` — assert; a mismatch traps.
    Assert,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssertKind {
    Assert,
    Eq,
    Ne,
    Throws,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CatchArm {
    pub binding: String,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LambdaParam {
    pub name: String,
    pub ty: Option<TypeSpec>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldInit {
    pub name: String,
    pub value: Expr,
    pub span: Span,
}

/// A list element: a value, or `..xs` splicing another list in.
#[derive(Debug, Clone, PartialEq)]
pub enum Elem {
    Value(Expr),
    Spread(Expr),
}

/// A string is literal text and interpolated expressions. The lexer emits these
/// as a flat token run; this is the reassembled tree.
#[derive(Debug, Clone, PartialEq)]
pub enum StrPart {
    Text(String),
    Interp(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub struct MatchArm {
    pub pat: Pattern,
    pub guard: Option<Expr>,
    pub body: Expr,
    pub span: Span,
}

// ---- patterns ----

#[derive(Debug, Clone, PartialEq)]
pub struct Pattern {
    pub kind: PatternKind,
    pub span: Span,
    /// Identity, from the same counter as [`Expr::id`] (see [`ids`]). A pattern is not an
    /// expression, but it *asks a type question* — `is List[str]`, `Circle { r }` — and the
    /// checker is the only thing that can answer it. Without a key of its own there was
    /// nowhere to record that answer, and lowering fell back to the pattern's syntax: the
    /// head name of the written path, which is exactly the information that makes
    /// `List[i64] is List[str]` come back true.
    pub id: ExprId,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PatternKind {
    /// `_`
    Wildcard,
    /// `x`
    Bind(String),
    /// `is T`
    Is(TypeSpec),
    /// `1`, `"s"`, `:ok`, `true`, `null`, `-1`. Boxed: `For` holds a Pattern
    /// inline, so an unboxed Expr here would close a cycle.
    Literal(Box<Expr>),
    /// `Point { x, y }` — field shorthand binds `x` to the field. `path: None`
    /// matches an anonymous record.
    Record { path: Option<Vec<String>>, fields: Vec<FieldPat>, rest: bool },
    /// `(a, b)`
    Tuple(Vec<Pattern>),
    Error,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FieldPat {
    pub name: String,
    /// `None` for the `{ x }` shorthand.
    pub pat: Option<Pattern>,
    pub span: Span,
}

// ---- operators ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    Bnot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Band,
    Bor,
    Bxor,
    Bsl,
    Bsr,
    /// Tests a nullable union's tag. Never "if truthy".
    Orelse,
    Pipe,
}
