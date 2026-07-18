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
    values: Vec<ValueData>,
}

#[derive(Debug, Clone)]
struct ValueData {
    repr: Repr,
    ty: TyId,
}

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
    /// Whether a nullable value is null. Codegen: a null-pointer or tag test.
    IsNull(Value),
    /// Whether a value is the named variant of a union (a nominal member, or a
    /// primitive kind by name). Codegen: a discriminant compare.
    IsVariant { value: Value, variant: String },
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

#[derive(Debug, Clone, PartialEq)]
pub enum Term {
    Ret(Option<Value>),
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
    pub fn values(&self) -> impl Iterator<Item = Value> + '_ {
        (0..self.values.len() as u32).map(Value)
    }
}

/// Builds one function incrementally: mint values and blocks, append instructions to
/// the current block, and finish blocks with a terminator. Lowering drives this.
pub struct Builder {
    name: String,
    ret: Repr,
    values: Vec<ValueData>,
    blocks: Vec<Block>,
    current: BlockId,
}

impl Builder {
    pub fn new(name: impl Into<String>, ret: Repr) -> Self {
        let entry = Block {
            id: BlockId(0),
            params: vec![],
            insts: vec![],
            term: Term::Unreachable,
        };
        Builder { name: name.into(), ret, values: vec![], blocks: vec![entry], current: BlockId(0) }
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

    /// A fresh empty block.
    pub fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(Block { id, params: vec![], insts: vec![], term: Term::Unreachable });
        id
    }

    /// Add a parameter to a block and return its value.
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

    /// Finish the current block with a terminator.
    pub fn terminate(&mut self, term: Term) {
        self.blocks[self.current.0 as usize].term = term;
    }

    /// Finish, declaring the entry block's parameters as the function's params.
    pub fn finish(self, params: Vec<Value>) -> Func {
        Func {
            name: self.name,
            params,
            ret: self.ret,
            entry: BlockId(0),
            blocks: self.blocks,
            values: self.values,
        }
    }
}

pub mod print;

#[cfg(test)]
mod tests;
