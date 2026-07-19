//! The SSA IR: functions of basic blocks, values defined once, joins expressed as
//! block arguments rather than φ-nodes. See `docs/design/ir.md`.
//!
//! A `Value` is an SSA temporary carrying both its `Repr` (for codegen) and its `TyId`
//! (for provenance and the effect analysis). A `Block` takes parameters, and every
//! predecessor passes them when it branches — a loop's carried state is the loop
//! header's parameters, and an `if`/`match` join is a block that takes the merged value.

use super::repr::Repr;
use crate::typecheck::types::TyId;

/// An SSA temporary, an index into a function's value table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub u32);

/// A basic block, an index into a function's block list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

/// A whole program: the monomorphic functions that make it up.
#[derive(Debug, Clone, Default)]
pub struct Program {
    pub funcs: Vec<Func>,
    /// Every recursive type in the program, paired with its unfolding. A back-edge is
    /// only an identity — `Repr::Recursive(ty)` says *which* type, not what it looks
    /// like — so the backend resolves it here to lay the type out and to refcount it.
    pub recursive: std::collections::HashMap<crate::typecheck::types::TyId, Repr>,
    /// Record atoms whose cycle closes entirely by value, paired with their pointee
    /// layout. Every value of such a record is a `Repr::BoxedRec` pointer; this is what it
    /// points at.
    pub boxed: std::collections::HashMap<u32, Repr>,
    /// The runtime symbols of natives declared `@pure`. A native not in this set is
    /// effectful; see the `@pure` processor in `expand.rs` for why that is the default.
    pub pure_natives: std::collections::HashSet<String>,
}

/// One function. `params` are the entry block's parameters; `values` records the repr
/// and type of every SSA value, indexed by `Value`.
#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub params: Vec<Value>,
    pub ret: Repr,
    pub entry: BlockId,
    pub blocks: Vec<Block>,
    /// For a lifted lambda, the repr of its environment (a tuple of the captures); the
    /// first parameter is that environment, passed boxed as a `neon_header*`. `None` for
    /// an ordinary function.
    pub env: Option<Repr>,
    /// The error repr of a throwing function. Such a function returns a tagged result
    /// rather than its declared type: variant 0 is the value, variant 1 the error.
    pub throws: Option<Repr>,
    values: Vec<ValueData>,
}

impl Func {
    /// The tagged result a throwing function actually returns — `{tag, union{ok, err}}`,
    /// expressed as a two-variant union so it shares the union layout and accessors.
    ///
    /// This is a *positional pair*, not a type-level union, and it is deliberately built
    /// raw rather than through `combine`/`normalize_union`. Those normalise: they dedupe
    /// variants, reorder them into canonical rank, and collapse `T | null` to a nullable
    /// pointer. Every one of those is wrong here. `IsErr`/`UnwrapOk`/`UnwrapErr` address
    /// the arms by index — 0 is the value, 1 is the error — so `fn f() throws str -> str`
    /// must stay a two-arm `Union([Str, Str])`, and `fn f() throws E -> null` must not
    /// become a nullable `E`. Normalising here would leave the accessors reading an arm
    /// that is no longer where they think it is.
    pub fn result_repr(&self) -> Option<Repr> {
        self.throws.as_ref().map(|e| Repr::Union(vec![self.ret.clone(), e.clone()]))
    }
}

#[derive(Debug, Clone)]
struct ValueData {
    repr: Repr,
    ty: TyId,
}

/// A basic block: parameters in, straight-line instructions, one terminator. Parameters
/// are where this IR puts what a φ-based IR puts at the top of a block, and the arguments
/// live on the *edge* (`Target`), so a value merged from two predecessors is named once and
/// a pass can insert code on one incoming edge without disturbing the other.
#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    pub term: Term,
}

/// One instruction: an operation and the value it defines, if any. A `Call` to a void
/// function or a `Release` defines nothing.
#[derive(Debug, Clone, PartialEq)]
pub struct Inst {
    pub result: Option<Value>,
    pub op: Op,
}

/// A primitive machine operation. The operands' `Repr` disambiguates `i64` from `f64`;
/// there is no separate `IAdd`/`FAdd`. `Orelse` and `Pipe` are not here — they desugar
/// during lowering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrimOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Not,
    Band,
    Bor,
    Bxor,
    Bsl,
    Bsr,
    Bnot,
}

/// What an instruction does. Deliberately close to what a C emitter can print in one line:
/// there is no addressing, no allocation op and no control flow here — control flow is
/// `Term`, and the refcount pass is the only thing that adds `Retain`/`Release`.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    ConstI64(i64),
    /// The bit pattern, so `Op` stays `Eq`/`Hash` for CSE — `f64` is neither.
    ConstF64(u64),
    ConstBool(bool),
    ConstStr(String),
    ConstNull,
    ConstUnit,
    /// An atom, by name; codegen hashes it to the runtime tag.
    ConstAtom(String),

    /// A primitive op on scalars.
    Prim(PrimOp, Vec<Value>),

    /// A direct call to a monomorphic function by its mangled name.
    Call { func: String, args: Vec<Value> },
    /// A call to a runtime symbol (a `@native`).
    Native { symbol: String, args: Vec<Value> },
    /// A call through a closure value.
    CallClosure { callee: Value, args: Vec<Value> },
    /// Build a closure: a function plus its captured environment.
    MakeClosure { func: String, captures: Vec<Value> },

    /// Build a record (nominal or anonymous), fields in declared order.
    MakeRecord { name: Option<String>, fields: Vec<(String, Value)> },
    /// Read a field.
    Field { base: Value, field: String },
    /// Build a tuple.
    MakeTuple(Vec<Value>),
    /// Read a tuple element.
    Elem { base: Value, index: usize },
    /// `x as T` — a reinterpretation to a narrower or wrapping type: identity at
    /// runtime for a narrowing or a newtype, an extraction out of a union.
    Cast(Value),
    /// A throwing call returns a tagged result. These read it: whether it is the error
    /// case, and the value out of each side.
    IsErr(Value),
    UnwrapOk(Value),
    UnwrapErr(Value),
    /// Whether a nullable value is null. Codegen: a null-pointer or tag test.
    IsNull(Value),
    /// Whether a value is the named variant of a union (a nominal member, or a
    /// primitive kind by name). Codegen: a discriminant compare.
    ///
    /// `variant` is the head name, which is all a *union* discriminant needs: a union's
    /// arms are distinct types, so naming one picks it out. `tested` is the checker's
    /// resolved type for the same test, and it is what an **erased** subject needs — a
    /// box's tag is derived from a `Repr`, so the only way for the two sides of
    /// `a is List[str]` to agree is for both to go through `TypeTable::type_tag`. The head
    /// name cannot: `List[i64]` and `List[str]` write the same one, so every `is` on an
    /// erased generic answered yes, and the `as` a person writes next reinterpreted the
    /// payload — an i64 read as a `neon_str` header, which is a segfault, not a wrong
    /// answer.
    IsVariant { value: Value, variant: String, tested: Option<Repr> },
    /// Build a list from its elements, in order.
    MakeList(Vec<Value>),
    /// Index a list — `xs[i]`, which traps on a bad index rather than throwing.
    Index { base: Value, index: Value },

    /// Retain / release, inserted by the refcount pass.
    Retain(Value),
    Release(Value),
}

/// Where a branch goes, and the block arguments it passes.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub to: BlockId,
    pub args: Vec<Value>,
}

/// How a block ends. Exactly one per block, and every block has one — a block under
/// construction carries `Unreachable` until `Builder::terminate` replaces it, so the IR is
/// never in a state with a missing terminator.
///
/// `Ret(None)` is a function returning nothing (a `Unit` return), not a missing value.
#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    Ret(Option<Value>),
    /// Return the error case of this throwing function's tagged result.
    Throw(Value),
    Jump(Target),
    Branch { cond: Value, then: Target, els: Target },
    Switch { on: Value, arms: Vec<(SwitchKey, Target)>, default: Target },
    /// Statically unreachable — after a call that never returns, or an exhausted match.
    Unreachable,
}

/// A `switch` arm's discriminant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchKey {
    Int(i64),
    Bool(bool),
    Atom(String),
    /// A nominal variant of a sum type, by name.
    Nominal(String),
}

// `Value` and `BlockId` are dense indices into `values` and `blocks`, minted only by
// `Builder`, so these lookups index directly and panic on an id from another function.
impl Func {
    pub fn value_repr(&self, v: Value) -> &Repr {
        &self.values[v.0 as usize].repr
    }
    pub fn value_ty(&self, v: Value) -> TyId {
        self.values[v.0 as usize].ty
    }
    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id.0 as usize]
    }
    /// Every value the function defines — instruction results, block parameters, and the
    /// unemitted `Never` values lowering mints for diverging expressions — in mint order.
    /// A pass wanting only the live ones must filter; nothing here prunes.
    pub fn values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.values.len() as u32).map(Value)
    }
}

/// Builds one function incrementally: mint values and blocks, append instructions to
/// the current block, and finish blocks with a terminator. Lowering drives this.
pub struct Builder {
    name: String,
    ret: Repr,
    /// Set when the function declares `throws`; makes its result a tagged union.
    throws: Option<Repr>,
    values: Vec<ValueData>,
    blocks: Vec<Block>,
    current: BlockId,
}

impl Builder {
    /// A builder with its entry block already made, so `BlockId(0)` is always the entry and
    /// callers can add the function's parameters to it before any lowering happens.
    pub fn new(name: impl Into<String>, ret: Repr) -> Self {
        let entry = Block {
            id: BlockId(0),
            params: vec![],
            insts: vec![],
            term: Term::Unreachable,
        };
        Builder {
            name: name.into(),
            ret,
            throws: None,
            values: vec![],
            blocks: vec![entry],
            current: BlockId(0),
        }
    }

    /// Mint a fresh value with a repr and type.
    pub fn value(&mut self, repr: Repr, ty: TyId) -> Value {
        let v = Value(self.values.len() as u32);
        self.values.push(ValueData { repr, ty });
        v
    }

    pub fn value_repr(&self, v: Value) -> &Repr {
        &self.values[v.0 as usize].repr
    }
    pub fn value_ty(&self, v: Value) -> TyId {
        self.values[v.0 as usize].ty
    }

    /// A fresh empty block. It starts `Unreachable` and does not become current: lowering
    /// routinely mints a join before it has anything to put in it, and `Unreachable` is the
    /// honest state for a block nothing has jumped to or filled in yet.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block { id, params: vec![], insts: vec![], term: Term::Unreachable });
        id
    }

    /// Add a parameter to a block and return its value. Parameters are appended, so every
    /// predecessor's `Target::args` must be built in the same order they were added here —
    /// there is no name to check them against.
    ///
    /// Nor is there a check on the reprs, and the invariant is weaker than it looks. An
    /// edge's argument repr is **not required to equal** the parameter's: lowering routinely
    /// passes a `str` and a `Null` into a `str?` join, and an `i64` into an `i64 | null`
    /// one, leaving the widen to the emitter. So the invariant is "every argument is
    /// *assignable* to its parameter", and that relation is not written down anywhere — it
    /// lives only in what the C emitter happens to accept. A verifier asserting equality
    /// would reject the corpus; one asserting assignability would first have to define it.
    /// Recording the gap here rather than claiming a check that does not exist.
    pub fn block_param(&mut self, block: BlockId, repr: Repr, ty: TyId) -> Value {
        let v = self.value(repr, ty);
        self.blocks[block.0 as usize].params.push(v);
        v
    }

    /// Switch the block that `emit`/`emit_void`/`terminate` append to.
    pub fn switch_to(&mut self, block: BlockId) {
        self.current = block;
    }

    pub fn current(&self) -> BlockId {
        self.current
    }

    /// Append an instruction that defines a value.
    pub fn emit(&mut self, op: Op, repr: Repr, ty: TyId) -> Value {
        let v = self.value(repr, ty);
        self.blocks[self.current.0 as usize].insts.push(Inst { result: Some(v), op });
        v
    }

    /// Append an instruction that defines nothing.
    pub fn emit_void(&mut self, op: Op) {
        self.blocks[self.current.0 as usize].insts.push(Inst { result: None, op });
    }

    /// Finish the current block with a terminator. This overwrites whatever was there, so
    /// a caller lowering a form that may already have terminated (a body ending in
    /// `return`) must check `Lower::terminated` first rather than relying on this.
    pub fn terminate(&mut self, term: Term) {
        self.blocks[self.current.0 as usize].term = term;
    }

    /// Finish, declaring the entry block's parameters as the function's params.
    pub fn finish(self, params: Vec<Value>) -> Func {
        self.finish_impl(params, None)
    }

    /// Finish a lifted lambda whose first parameter is a boxed environment of `env` repr.
    pub fn finish_lambda(self, params: Vec<Value>, env: Repr) -> Func {
        self.finish_impl(params, Some(env))
    }

    /// Record that this function throws, so its result becomes the tagged
    /// `Union([ret, err])` that `Func::result_repr` spells out. `err` must never be
    /// `Repr::Never`: a `never` clause means the function does not throw, and setting it
    /// anyway would tag a result the callers read untagged.
    ///
    /// The rule was a comment for as long as it existed, which is the shape of bug this
    /// audit keeps finding: an invariant stated where nobody can check it. It is an
    /// assertion now, so a caller that gets it wrong stops rather than silently producing a
    /// function whose result the whole program then reads at the wrong layout.
    pub fn set_throws(&mut self, err: Repr) {
        debug_assert!(
            err != Repr::Never,
            "a `throws never` clause means the function does not throw; tagging its result \
             would make every caller read a value that is not there",
        );
        self.throws = Some(err);
    }

    /// The declared error repr, if this function throws.
    pub fn throws(&self) -> Option<&Repr> {
        self.throws.as_ref()
    }

    /// Retype a value. Used for a throwing call's result: the call is emitted at the
    /// callee's declared return, then retyped to the tagged result it actually yields, so
    /// codegen and the refcount pass agree about what the value holds.
    pub fn set_value_repr(&mut self, v: Value, repr: Repr) {
        self.values[v.0 as usize].repr = repr;
    }

    fn finish_impl(self, params: Vec<Value>, env: Option<Repr>) -> Func {
        Func {
            name: self.name,
            params,
            ret: self.ret,
            entry: BlockId(0),
            blocks: self.blocks,
            env,
            throws: self.throws,
            values: self.values,
        }
    }
}

pub mod print;

#[cfg(test)]
mod tests;
