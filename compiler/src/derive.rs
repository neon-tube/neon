//! `@derive(P)` — generating an impl a user could have written by hand.
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
//!
//! # Adding one
//!
//! Write a unit struct, implement [`Derivable`] on it, and add it to [`DERIVABLE`]. That is
//! the whole list of edits, and it is one place because everything else — whether a name is
//! derivable, which generator runs, and the set a diagnostic names — reads the registry.

use crate::ast::{
    Annotation, Block, Decl, DeclKind, Elem, Expr, ExprId, ExprKind, FnDecl, ImplDecl, Module,
    Param, RecordDecl, StrPart, TypeSpec, TypeSpecKind, WhereClause,
};
use crate::lexer::Span;

/// One protocol the compiler knows how to write an impl for.
///
/// The same shape as `expand.rs`'s `Processor`, and for the same reason: a derive is a rule,
/// not an object, so it is a unit struct with a method rather than data. Adding one is
/// writing the generator and putting it in [`DERIVABLE`] — there is no second list to
/// update and no way for two lists to disagree.
///
/// That mattered. `can_derive` and the generator used to be separate `match`es over the same
/// names, with a doc comment asking whoever added a derive to keep them in step, and
/// `expand.rs` spelled the set a third time in prose inside a diagnostic. Three lists, one
/// fact. `ToJson` shipped with all three edited by hand; the next one would not have been.
trait Derivable {
    /// The protocol name this writes an impl for — the LAST segment of what the author
    /// wrote, for the reason [`can_derive`] gives.
    fn protocol(&self) -> &'static str;

    /// Write the impl. `protocol` is the author's whole path, carried verbatim into the
    /// generated decl; `span` is the annotation argument's, which is what keeps two derives
    /// on one record distinguishable (see `derive_decls`). `template` is the string a derive
    /// may carry -- only `Error`'s does; the rest ignore it, and `expand` has already
    /// rejected one given to a protocol that has no use for it.
    fn write(
        &self,
        protocol: &[String],
        r: &RecordDecl,
        ann: &Annotation,
        span: &Span,
        template: Option<&str>,
    ) -> Decl;
}

/// The registry. A protocol not here is not derivable, and that is the whole definition —
/// `can_derive`, the dispatch in `derive_one` and the diagnostic's list of names all read
/// it rather than restating it.
const DERIVABLE: &[&dyn Derivable] = &[&Display, &ToJson, &FromJson, &ErrorMessage];

fn lookup(name: &str) -> Option<&'static dyn Derivable> {
    DERIVABLE.iter().copied().find(|d| d.protocol() == name)
}

/// Every derivable protocol's name, for a diagnostic that has to list them. Generated from
/// the registry so a new derive cannot ship with a message that omits it.
pub fn derivable_names() -> Vec<&'static str> {
    DERIVABLE.iter().map(|d| d.protocol()).collect()
}

/// Whether `@derive(path)` names something this pass can write.
///
/// Matched on the **last segment**, and the generated impl carries the whole path the author
/// wrote. So `@derive(Display)` and `@derive(std::fmt::Display)` both land here, and which
/// protocol the impl is actually *for* is settled where every other protocol name is settled
/// — by resolution, on the generated impl. This pass runs before the checker and resolves
/// nothing; pretending otherwise would mean a second name resolver that agreed with the real
/// one only by luck.
///
/// The cost is that `@derive(mine::Display)`, naming some other protocol that happens to end
/// in `Display`, gets a body written for the wrong protocol. That is a type error against the
/// generated impl rather than a silent wrong answer, which is the same deal a hand-written
/// impl of the wrong protocol gets.
pub fn can_derive(path: &[String]) -> bool {
    path.last().is_some_and(|n| lookup(n).is_some())
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
                    for arg in &ann.args {
                        let Some((path, template)) = arg.derive_arg() else {
                            continue;
                        };
                        // The ARGUMENT's span, not the record's, and it is load-bearing
                        // rather than a nicety. `lower.rs::impl_def_at` correlates an impl's
                        // AST with its `ImplDef` by `(module, span)` and takes the first
                        // match, so two impls derived onto one record from the record's span
                        // are indistinguishable there: the second one's methods get indexed
                        // under the FIRST one's protocol -- `ToJson$P$from_json`, a key
                        // nothing looks up -- and its body is silently never lowered.
                        //
                        // Invisible until there were two derivable protocols, because with
                        // only `Display` a record could not carry two derives. Each argument
                        // has its own span whether the author wrote `@derive(A) @derive(B)`
                        // or `@derive(A, B)`, so this distinguishes both spellings, and it
                        // points a diagnostic about the generated impl at the name that
                        // asked for it.
                        if let Some(decl) = derive_one(path, template, r, ann, arg.span()) {
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

/// Hand the whole path to the generator so the impl is written for the protocol the author
/// named rather than for the compiler's idea of where it lives.
///
/// The path is carried VERBATIM. A bare `@derive(ToJson)` in a module that never imported
/// `ToJson` is "unknown protocol `ToJson`", which is exactly what a hand-written
/// `impl ToJson for P` would say there — a protocol name resolves the way every other name
/// resolves, through the author's imports, and `@derive` gets no private route. So the three
/// spellings are the three you would use anywhere else:
///
/// ```text
/// use std::json;             @derive(json::ToJson)
/// use std::json::ToJson;     @derive(ToJson)
///                            @derive(std::json::ToJson)
/// ```
///
/// `Display` needs none of them only because it is in the prelude, which is a fact about the
/// prelude and not a privilege of the derive.
fn derive_one(
    path: &[String],
    template: Option<&str>,
    r: &RecordDecl,
    ann: &Annotation,
    span: &Span,
) -> Option<Decl> {
    // `expand` rejected anything not in the registry before we got here.
    let d = lookup(path.last()?)?;
    Some(d.write(path, r, ann, span, template))
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
struct Display;

impl Derivable for Display {
    fn protocol(&self) -> &'static str {
        "Display"
    }

    fn write(
        &self,
        protocol: &[String],
        r: &RecordDecl,
        ann: &Annotation,
        span: &Span,
        _template: Option<&str>,
    ) -> Decl {
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
                let lead = if i == 0 {
                    String::new()
                } else {
                    ", ".to_string()
                };
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
            params: vec![Param {
                name: "v".to_string(),
                ty: target.clone(),
                span: sp(),
            }],
            throws: None,
            ret: Some(named("str", vec![], &sp())),
            wheres: vec![],
            body: Some(body),
            annotations: vec![],
        };

        // The bound is spelled with the author's path too, so `where T: Display` and a qualified
        // `where T: std::fmt::Display` resolve to whatever the impl's own protocol resolved to.
        let bound = TypeSpec {
            kind: TypeSpecKind::Named {
                path: protocol.to_vec(),
                args: vec![],
            },
            span: sp(),
        };
        let wheres = r
            .generics
            .iter()
            .map(|g| WhereClause {
                param: g.clone(),
                bound: bound.clone(),
            })
            .collect();

        Decl {
            docs: Vec::new(),
            kind: DeclKind::Impl(ImplDecl {
                orphan: false,
                protocol: protocol.to_vec(),
                generics: r.generics.clone(),
                target,
                wheres,
                methods: vec![method],
                annotations: vec![],
            }),
            span: span.clone(),
        }
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
        kind: TypeSpecKind::Named {
            path: vec![name.to_string()],
            args,
        },
        span: span.clone(),
    }
}

/// `impl[T] ToJson for P[T] where T: ToJson { fn to_json(v: P[T]) -> std::json::Json { .. } }`
///
/// The body is one call:
///
/// ```text
/// std::json::object([("x", to_json(v.x)), ("y", to_json(v.y))])
/// ```
///
/// which is the `Display` trick in the shape a structured document needs. There, each field
/// became an interpolation hole and the hole did the dispatch; here each field becomes an
/// ordinary `to_json` call and dispatch does the same work. So this pass again knows nothing
/// about field types: a nested derived record, a `List[T]` with a library impl and a
/// primitive all encode through whatever impl covers them, and a field whose type has no
/// `ToJson` is a dispatch error naming that type.
///
/// Field ORDER is not preserved, and cannot be: `object` builds a `Map`, and `stringify`
/// sorts keys because a map has no order to recover. Emitting fields in declaration order
/// here would be discarded three lines later. See `std::json`'s header for why sorted is
/// the honest choice rather than a limitation to work around.
///
/// The two support names — the `Json` type and the `object` builder — are written as
/// ABSOLUTE paths, unlike the protocol, which keeps the author's spelling. The protocol is a
/// name the author chose and resolution must settle (see `can_derive`); these two are this
/// pass's own scaffolding, and a generated body that only compiled when the author happened
/// to `use std::json` would break on someone else's file. Absolute paths resolve with no
/// import, so `@derive(ToJson)` works in a module that imports nothing.
struct ToJson;

impl Derivable for ToJson {
    fn protocol(&self) -> &'static str {
        "ToJson"
    }

    fn write(
        &self,
        protocol: &[String],
        r: &RecordDecl,
        ann: &Annotation,
        span: &Span,
        _template: Option<&str>,
    ) -> Decl {
        let sp = || ann.span.clone();
        let target = self_type(r, &sp());

        // `[("x", to_json(v.x)), ..]` — one tuple per field, in declaration order. The order is
        // not load-bearing (see above); it just makes the generated source readable when dumped.
        let fields: Vec<Elem> = r
            .fields
            .iter()
            .map(|f| {
                let name = expr(ExprKind::Str(vec![StrPart::Text(f.name.clone())]), &sp());
                let call = expr(
                    ExprKind::Call {
                        callee: Box::new(expr(ExprKind::Path(vec!["to_json".to_string()]), &sp())),
                        generics: vec![],
                        args: vec![field_of("v", &f.name, &sp())],
                    },
                    &sp(),
                );
                Elem::Value(expr(ExprKind::Tuple(vec![name, call]), &sp()))
            })
            .collect();

        let call = expr(
            ExprKind::Call {
                callee: Box::new(expr(ExprKind::Path(json_path("object")), &sp())),
                generics: vec![],
                args: vec![expr(ExprKind::List(fields), &sp())],
            },
            &sp(),
        );

        let body = Block {
            stmts: vec![],
            tail: Some(Box::new(call)),
            span: sp(),
        };

        let method = FnDecl {
            name: "to_json".to_string(),
            generics: vec![],
            params: vec![Param {
                name: "v".to_string(),
                ty: target.clone(),
                span: sp(),
            }],
            throws: None,
            ret: Some(TypeSpec {
                kind: TypeSpecKind::Named {
                    path: json_path("Json"),
                    args: vec![],
                },
                span: sp(),
            }),
            wheres: vec![],
            body: Some(body),
            annotations: vec![],
        };

        // As in `display_impl`: the bound carries the author's protocol path, so a qualified
        // `@derive(json::ToJson)` bounds its parameters by the same protocol the impl is for.
        let bound = TypeSpec {
            kind: TypeSpecKind::Named {
                path: protocol.to_vec(),
                args: vec![],
            },
            span: sp(),
        };
        let wheres = r
            .generics
            .iter()
            .map(|g| WhereClause {
                param: g.clone(),
                bound: bound.clone(),
            })
            .collect();

        Decl {
            docs: Vec::new(),
            kind: DeclKind::Impl(ImplDecl {
                orphan: false,
                protocol: protocol.to_vec(),
                generics: r.generics.clone(),
                target,
                wheres,
                methods: vec![method],
                annotations: vec![],
            }),
            span: span.clone(),
        }
    }
}

/// `impl[T] FromJson for P[T] where T: FromJson`, the mirror of `to_json_impl`:
///
/// ```text
/// fn from_json(j: Json) throws JsonError -> P[T] {
///     P { x: try from_json(try std::json::field(j, "x")), .. }
/// }
/// ```
///
/// The record literal is what makes this work without the pass knowing any field's type.
/// `from_json` dispatches on the type its result is CHECKED against, and a literal's field
/// slot supplies exactly that — so `x`'s decode picks `x`'s impl, whatever it is, and a
/// field whose type has no `FromJson` is a dispatch error naming that type.
///
/// That is the same trick as the other two derives and a different mechanism: `Display`
/// dispatches on an interpolation hole's value, `ToJson` on `to_json`'s argument, and this
/// on nothing that is passed at all. It is the reason return-position dispatch had to work
/// before a decode derive was expressible.
///
/// Every call is a `try`, and the method `throws JsonError`, so a malformed document
/// propagates the first failure with the field name already in its message.
struct FromJson;

impl Derivable for FromJson {
    fn protocol(&self) -> &'static str {
        "FromJson"
    }

    fn write(
        &self,
        protocol: &[String],
        r: &RecordDecl,
        ann: &Annotation,
        span: &Span,
        _template: Option<&str>,
    ) -> Decl {
        let sp = || ann.span.clone();
        let target = self_type(r, &sp());

        let fields = r
            .fields
            .iter()
            .map(|f| {
                // `try std::json::field(j, "x")` — the node, or a throw naming the missing key.
                let name = expr(ExprKind::Str(vec![StrPart::Text(f.name.clone())]), &sp());
                let node = try_(
                    expr(
                        ExprKind::Call {
                            callee: Box::new(expr(ExprKind::Path(json_path("field")), &sp())),
                            generics: vec![],
                            args: vec![expr(ExprKind::Path(vec!["j".to_string()]), &sp()), name],
                        },
                        &sp(),
                    ),
                    &sp(),
                );
                // `try from_json(<node>)`, dispatched by this field's declared type.
                let decoded = try_(
                    expr(
                        ExprKind::Call {
                            callee: Box::new(expr(
                                ExprKind::Path(vec!["from_json".to_string()]),
                                &sp(),
                            )),
                            generics: vec![],
                            args: vec![node],
                        },
                        &sp(),
                    ),
                    &sp(),
                );
                crate::ast::FieldInit {
                    name: f.name.clone(),
                    value: decoded,
                    span: sp(),
                }
            })
            .collect();

        let lit = expr(
            ExprKind::RecordLit {
                path: Some(vec![r.name.clone()]),
                fields,
                spread: None,
            },
            &sp(),
        );
        let body = Block {
            stmts: vec![],
            tail: Some(Box::new(lit)),
            span: sp(),
        };

        let method = FnDecl {
            name: "from_json".to_string(),
            generics: vec![],
            params: vec![Param {
                name: "j".to_string(),
                ty: TypeSpec {
                    kind: TypeSpecKind::Named {
                        path: json_path("Json"),
                        args: vec![],
                    },
                    span: sp(),
                },
                span: sp(),
            }],
            throws: Some(TypeSpec {
                kind: TypeSpecKind::Named {
                    path: json_path("JsonError"),
                    args: vec![],
                },
                span: sp(),
            }),
            ret: Some(target.clone()),
            wheres: vec![],
            body: Some(body),
            annotations: vec![],
        };

        let bound = TypeSpec {
            kind: TypeSpecKind::Named {
                path: protocol.to_vec(),
                args: vec![],
            },
            span: sp(),
        };
        let wheres = r
            .generics
            .iter()
            .map(|g| WhereClause {
                param: g.clone(),
                bound: bound.clone(),
            })
            .collect();

        Decl {
            docs: Vec::new(),
            kind: DeclKind::Impl(ImplDecl {
                orphan: false,
                protocol: protocol.to_vec(),
                generics: r.generics.clone(),
                target,
                wheres,
                methods: vec![method],
                annotations: vec![],
            }),
            span: span.clone(),
        }
    }
}

/// `impl Error for P { fn message(v: P) -> str { "..#{v.field}.." } }` — the message ASSEMBLED
/// from a template rather than stored. `@derive(Error("cannot open {path}: {reason}"))` reads
/// each `{field}` as a hole over that field, so the message is computed from the record's own
/// fields whenever it is asked for -- no redundant rendered copy kept in a field, which is the
/// whole reason a field-reading derive was the wrong shape. Bare `@derive(Error)` renders a
/// single `message` field, for an error that carries only that and wants the boilerplate gone.
///
/// A field renders through `Display` because it is an interpolation hole. So an unknown
/// `{field}` is the same "no field" error a hand-written `v.field` gives, and a field whose
/// type has no `Display` is a dispatch error naming it -- exactly what writing the body out by
/// hand would have hit. `{{` and `}}` are literal braces, as under `#{ }`.
struct ErrorMessage;

impl Derivable for ErrorMessage {
    fn protocol(&self) -> &'static str {
        "Error"
    }

    fn write(
        &self,
        protocol: &[String],
        r: &RecordDecl,
        ann: &Annotation,
        span: &Span,
        template: Option<&str>,
    ) -> Decl {
        let sp = || ann.span.clone();
        let target = self_type(r, &sp());

        let parts = match template {
            Some(t) => parse_template(t, &sp()),
            // Bare `@derive(Error)`: render the `message` field verbatim. An error with no such
            // field and no template is a "no field `message`" error against this body, which is
            // the nudge to give the derive a template or the record a `message`.
            None => vec![StrPart::Interp(field_of("v", "message", &sp()))],
        };

        let body = Block {
            stmts: vec![],
            tail: Some(Box::new(expr(ExprKind::Str(parts), &sp()))),
            span: sp(),
        };

        let method = FnDecl {
            name: "message".to_string(),
            generics: vec![],
            params: vec![Param {
                name: "v".to_string(),
                ty: target.clone(),
                span: sp(),
            }],
            throws: None,
            ret: Some(named("str", vec![], &sp())),
            wheres: vec![],
            body: Some(body),
            annotations: vec![],
        };

        // A generic field renders through `Display` in a hole, so bound each parameter by it.
        // Every error record today is non-generic, so this is empty in practice.
        let display_bound = named("Display", vec![], &sp());
        let wheres = r
            .generics
            .iter()
            .map(|g| WhereClause {
                param: g.clone(),
                bound: display_bound.clone(),
            })
            .collect();

        Decl {
            docs: Vec::new(),
            kind: DeclKind::Impl(ImplDecl {
                orphan: false,
                protocol: protocol.to_vec(),
                generics: r.generics.clone(),
                target,
                wheres,
                methods: vec![method],
                annotations: vec![],
            }),
            span: span.clone(),
        }
    }
}

/// A message template into the pieces an interpolated string is built from: literal text, and
/// `{field}` holes that read and render a field. `{{`/`}}` are literal braces. A `{` with no
/// closing `}` takes the rest of the string as the field name, which then fails as "no field"
/// against whatever it spells -- the diagnostic a malformed template deserves.
fn parse_template(t: &str, span: &Span) -> Vec<StrPart> {
    let mut parts: Vec<StrPart> = Vec::new();
    let mut text = String::new();
    let mut chars = t.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' if chars.peek() == Some(&'{') => {
                chars.next();
                text.push('{');
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
                text.push('}');
            }
            '{' => {
                if !text.is_empty() {
                    parts.push(StrPart::Text(std::mem::take(&mut text)));
                }
                let mut name = String::new();
                while let Some(&c2) = chars.peek() {
                    if c2 == '}' {
                        break;
                    }
                    name.push(c2);
                    chars.next();
                }
                chars.next(); // the closing '}', if present
                parts.push(StrPart::Interp(field_of("v", name.trim(), span)));
            }
            _ => text.push(c),
        }
    }
    if !text.is_empty() {
        parts.push(StrPart::Text(text));
    }
    parts
}

/// `try e` — the propagating form, which is the only one a derive wants: a decode that
/// fails should surface the failure, not soften it to null or abort the process.
fn try_(e: Expr, span: &Span) -> Expr {
    expr(
        ExprKind::Try {
            form: crate::ast::TryForm::Propagate,
            body: Box::new(e),
            catch: None,
        },
        span,
    )
}

/// A name in `std::json`, spelled absolutely. See `to_json_impl` for why these are not
/// derived from the author's path the way the protocol is.
fn json_path(name: &str) -> Vec<String> {
    vec!["std".to_string(), "json".to_string(), name.to_string()]
}

fn field_of(base: &str, field: &str, span: &Span) -> Expr {
    let b = expr(ExprKind::Path(vec![base.to_string()]), span);
    expr(
        ExprKind::Field {
            base: Box::new(b),
            name: field.to_string(),
        },
        span,
    )
}

fn expr(kind: ExprKind, span: &Span) -> Expr {
    Expr {
        kind,
        span: span.clone(),
        id: ExprId::UNSET,
    }
}
