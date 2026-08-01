//! `@derive("Display")` — generating an impl a user could have written by hand.
//!
//! This is the one place in the compiler that *adds* declarations, and it is a separate
//! pass rather than an annotation processor for that reason. `expand`'s `Context`
//! deliberately cannot see the AST, which is what stops annotations from becoming a macro
//! system (see `docs/design/annotations.md`); a processor may only keep or omit the node it
//! sits on. So `@derive` is split: `expand` validates the annotation — right target, an
//! argument, a protocol the compiler knows how to write — and this pass, running after it,
//! does the writing. By the time we get here the annotation is known to be well-formed.
//!
//! What it generates is an ORDINARY impl. Not a marker the checker treats specially, not a
//! backend intrinsic: the same declarations the author could have typed, appended to the
//! module the record lives in, type-checked and lowered like any other code. That is what
//! makes a derive overridable by simply writing the impl yourself, and it is why a derived
//! impl's mistakes are reported against the record rather than swallowed.
//!
//! It runs BEFORE `number_exprs_from`, so everything built here carries `ExprId::UNSET` and
//! is numbered with the rest of the module. Nothing here has to know about expression ids.

use crate::ast::{
    Annotation, Block, Decl, DeclKind, Expr, ExprId, ExprKind, FnDecl, ImplDecl, Module, Param,
    RecordDecl, StrPart, TypeSpec, TypeSpecKind, WhereClause,
};
use crate::lexer::Span;

/// The protocols `@derive` knows how to write. A name outside this list is rejected by
/// `expand`, so the pass below never meets one.
pub fn can_derive(name: &str) -> bool {
    name == "Display"
}

/// The argument of `@derive`, split. The arg is an opaque string like every annotation's
/// (`docs/design/annotations.md`: "a processor brings its own parser"), so `@derive("Display,
/// Eq")` is one annotation naming two protocols rather than a second syntax.
pub fn protocols(arg: &str) -> Vec<String> {
    arg.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Append a generated impl for every `@derive` in the module, recursing into nested mods.
pub fn derive(module: &mut Module) {
    derive_decls(&mut module.decls);
}

fn derive_decls(decls: &mut Vec<Decl>) {
    let mut generated: Vec<Decl> = Vec::new();
    for d in decls.iter_mut() {
        match &mut d.kind {
            DeclKind::Record(r) => {
                for ann in &r.annotations {
                    if ann.name != "derive" {
                        continue;
                    }
                    let Some(arg) = &ann.arg else { continue };
                    for name in protocols(arg) {
                        if let Some(decl) = derive_one(&name, r, ann, &d.span) {
                            generated.push(decl);
                        }
                    }
                }
            }
            DeclKind::Mod(m) => derive_decls(&mut m.decls),
            _ => {}
        }
    }
    decls.extend(generated);
}

fn derive_one(protocol: &str, r: &RecordDecl, ann: &Annotation, span: &Span) -> Option<Decl> {
    match protocol {
        "Display" => Some(display_impl(r, ann, span)),
        // `expand` rejected everything else before we got here.
        _ => None,
    }
}

/// `impl[T] Display for P[T] where T: Display { fn to_string(v: P[T]) -> str { .. } }`
///
/// The body is an interpolated string, which is the whole trick: each field becomes a hole,
/// and a hole already dispatches `Display` on whatever it holds. So a field of a derived
/// record, of a `List[T]` with a library impl, or of a primitive all work without this pass
/// knowing anything about them — and a field whose type has no `Display` is a dispatch
/// error naming that field's type, which is the diagnostic a hand-written impl would give.
///
/// A generic record derives a bounded impl, `where T: Display` per parameter. That is not
/// decoration: without it the body cannot call `to_string` on a field of type `T` at all,
/// because inside the impl `T` is rigid and only a bound can answer for it.
fn display_impl(r: &RecordDecl, ann: &Annotation, span: &Span) -> Decl {
    let sp = || ann.span.clone();
    let target = self_type(r, &sp());

    // `"P { x: #{v.x}, y: #{v.y} }"`, assembled part by part. An empty record renders as
    // `P {}`, mirroring how one is written.
    let mut parts: Vec<StrPart> = Vec::new();
    if r.fields.is_empty() {
        parts.push(StrPart::Text(format!("{} {{}}", r.name)));
    } else {
        parts.push(StrPart::Text(format!("{} {{ ", r.name)));
        for (i, f) in r.fields.iter().enumerate() {
            let lead = if i == 0 { String::new() } else { ", ".to_string() };
            parts.push(StrPart::Text(format!("{lead}{}: ", f.name)));
            parts.push(StrPart::Interp(field_of("v", &f.name, &sp())));
        }
        parts.push(StrPart::Text(" }".to_string()));
    }

    let body = Block {
        stmts: vec![],
        tail: Some(Box::new(expr(ExprKind::Str(parts), &sp()))),
        span: sp(),
    };

    let method = FnDecl {
        name: "to_string".to_string(),
        generics: vec![],
        params: vec![Param { name: "v".to_string(), ty: target.clone(), span: sp() }],
        throws: None,
        ret: Some(named("str", vec![], &sp())),
        wheres: vec![],
        body: Some(body),
        annotations: vec![],
    };

    let wheres = r
        .generics
        .iter()
        .map(|g| WhereClause { param: g.clone(), bound: named("Display", vec![], &sp()) })
        .collect();

    Decl {
        kind: DeclKind::Impl(ImplDecl {
            orphan: false,
            protocol: vec!["Display".to_string()],
            generics: r.generics.clone(),
            target,
            wheres,
            methods: vec![method],
            annotations: vec![],
        }),
        span: span.clone(),
    }
}

/// The record as a type, applied to its own parameters: `P` for a plain record, `P[T, U]`
/// for a generic one. Written with the parameters as arguments, not left bare, because a
/// bare constructor is a different kind of impl target entirely (`impl Container for Box`).
fn self_type(r: &RecordDecl, span: &Span) -> TypeSpec {
    let args = r.generics.iter().map(|g| named(g, vec![], span)).collect();
    named(&r.name, args, span)
}

fn named(name: &str, args: Vec<TypeSpec>, span: &Span) -> TypeSpec {
    TypeSpec {
        kind: TypeSpecKind::Named { path: vec![name.to_string()], args },
        span: span.clone(),
    }
}

fn field_of(base: &str, field: &str, span: &Span) -> Expr {
    let b = expr(ExprKind::Path(vec![base.to_string()]), span);
    expr(ExprKind::Field { base: Box::new(b), name: field.to_string() }, span)
}

fn expr(kind: ExprKind, span: &Span) -> Expr {
    Expr { kind, span: span.clone(), id: ExprId::UNSET }
}
