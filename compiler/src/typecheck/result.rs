use super::dispatch::Resolution;
use super::types::TyId;
use crate::ast::ExprId;
use std::collections::HashMap;

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
    /// A lambda's inferred signature, as an arrow.
    resolved_lambdas: HashMap<ExprId, TyId>,
}

impl TypecheckResult {
    pub fn ty(&self, e: ExprId) -> Option<TyId> {
        self.expr_types.get(&e).copied()
    }

    pub fn call(&self, e: ExprId) -> Option<&Resolution> {
        self.resolved_calls.get(&e)
    }

    pub fn lambda(&self, e: ExprId) -> Option<TyId> {
        self.resolved_lambdas.get(&e).copied()
    }

    pub fn len(&self) -> usize {
        self.expr_types.len()
    }

    pub fn is_empty(&self) -> bool {
        self.expr_types.is_empty()
    }

    pub(super) fn set_ty(&mut self, e: ExprId, t: TyId) {
        self.expr_types.insert(e, t);
    }

    pub(super) fn set_call(&mut self, e: ExprId, r: Resolution) {
        self.resolved_calls.insert(e, r);
    }

    /// No caller yet: `Lambda` is one of the forms the checker does not infer.
    #[allow(dead_code)]
    pub(super) fn set_lambda(&mut self, e: ExprId, t: TyId) {
        self.resolved_lambdas.insert(e, t);
    }
}
