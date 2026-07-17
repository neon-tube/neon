//! `TyId` -> readable Neon type syntax.
//!
//! The residual of `s ∧ ¬covered` is the diagnostic, so this is what makes the
//! set-theoretic representation say what it knows.
//!
//! Not every type is writable. A name in angle brackets — `<absent>`, `<atom>`,
//! `<record>`, `<...>` — is one this printer had to name and source cannot: `#` and
//! `<` are not identifier characters, so such a rendering can never be mistaken for
//! a type that parses. Preferring one to a plausible-looking lie is the rule here.
//! `never` and `mu A0 = ...` are the same bargain in readable clothing: both are the
//! design's own vocabulary, and neither is source syntax.

use super::types::*;
use std::fmt::Write;

/// Mirrors the parser's `bool_type` and the formatter's `TP_*`: `!` tightest, then
/// `&`, then `|`. An arrow sits at the bottom with `mu`, so both parenthesise
/// anywhere but the top.
const P_ANY: u8 = 0;
const P_UNION: u8 = 1;
const P_INTERSECT: u8 = 2;
const P_NEGATE: u8 = 3;
const P_ATOM: u8 = 4;

const ABSENT: &str = "<absent>";
const ALL_ATOMS: &str = "<atom>";
const ALL_VARS: &str = "<var>";
const ALL_RECORDS: &str = "<record>";
const ALL_TUPLES: &str = "<tuple>";
const ALL_ARROWS: &str = "<fn>";
const CUT: &str = "<...>";

/// Guards against a stack overflow on a deep type, and against the cost of the
/// negation heuristic on a wide one. Cycles are cut by the `mu` stack, not by this.
const MAX_DEPTH: usize = 32;

/// A hard cap on rendered nodes. Sharing makes a finite type graph exponential to
/// print; a diagnostic that hangs the compiler is worse than a truncated one.
const MAX_NODES: usize = 4096;

/// `ty` as Neon type syntax.
///
/// Takes `&mut` because deciding whether to print a type or its complement means
/// interning the complement. Nothing observable is added: the table is hash-consed.
pub fn print(t: &mut Types, ty: TyId) -> String {
    let mut p = Printer { t, stack: vec![], used: vec![], budget: MAX_NODES };
    let r = p.render(ty);
    at(P_ANY, r)
}

struct Printer<'a> {
    t: &'a mut Types,
    /// The ids being rendered, outermost first. An id reached while it is here is a
    /// cycle, and its index is its `mu` name.
    stack: Vec<TyId>,
    used: Vec<bool>,
    budget: usize,
}

fn at(min: u8, (s, p): (String, u8)) -> String {
    if p < min { format!("({s})") } else { s }
}

fn join(parts: Vec<(String, u8)>, sep: &str, inner: u8, outer: u8) -> (String, u8) {
    if parts.len() == 1 {
        return parts.into_iter().next().expect("len 1");
    }
    let mut s = String::new();
    for (i, part) in parts.into_iter().enumerate() {
        if i > 0 {
            s.push_str(sep);
        }
        s.push_str(&at(inner, part));
    }
    (s, outer)
}

fn union(parts: Vec<(String, u8)>) -> (String, u8) {
    join(parts, " | ", P_INTERSECT, P_UNION)
}

fn intersect(parts: Vec<(String, u8)>) -> (String, u8) {
    join(parts, " & ", P_NEGATE, P_INTERSECT)
}

fn mu_name(k: usize) -> String {
    format!("A{k}")
}

impl Printer<'_> {
    fn render(&mut self, id: TyId) -> (String, u8) {
        if let Some(k) = self.stack.iter().position(|&x| x == id) {
            self.used[k] = true;
            return (mu_name(k), P_ATOM);
        }
        if let Some(n) = self.def_name(id) {
            return (n, P_ATOM);
        }
        if self.stack.len() >= MAX_DEPTH || self.budget == 0 {
            return (CUT.to_string(), P_ATOM);
        }
        self.budget -= 1;

        let k = self.stack.len();
        self.stack.push(id);
        self.used.push(false);
        let body = self.body(id);
        self.stack.pop();
        let used = self.used.pop().unwrap_or(false);

        if used {
            // Not source syntax: `mu` introduces a declaration, never a type
            // expression. An unnameable cycle is named rather than expanded.
            (format!("mu {} = {}", mu_name(k), at(P_ANY, body)), P_ANY)
        } else {
            body
        }
    }

    fn ty(&mut self, id: TyId, min: u8) -> String {
        let r = self.render(id);
        at(min, r)
    }

    fn def_name(&self, id: TyId) -> Option<String> {
        // Hash-consing lets several names reach one id; the least is stable.
        self.t
            .defs
            .iter()
            .filter(|(_, &t)| t == id)
            .map(|(&n, _)| n)
            .min()
            .map(|n| self.t.name_str(n).to_string())
    }

    fn body(&mut self, id: TyId) -> (String, u8) {
        let d = self.t.data(id);
        // The absent marker is not a value, so it is split off before anything asks
        // what set of values this is.
        let vd = TyData { base: d.base & B_ANY, ..d };
        let vid = self.t.intern(vd);
        let (never, any) = (self.t.never(), self.t.any());

        let mut parts = Vec::new();
        if vid == any {
            parts.push(("any".to_string(), P_ATOM));
        } else if vid != never {
            match self.complement(vid, vd) {
                Some(p) => parts.push(p),
                None => parts.extend(self.positive(vd)),
            }
        }
        if d.base & B_UNDEF != 0 {
            parts.push((ABSENT.to_string(), P_ATOM));
        }
        if parts.is_empty() {
            return ("never".to_string(), P_ATOM);
        }
        union(parts)
    }

    /// `!c` when `c` is the smaller of the two. Without this, `!:ok` prints as every
    /// primitive, every other atom, every record, every tuple and every arrow.
    fn complement(&mut self, vid: TyId, vd: TyData) -> Option<(String, u8)> {
        // A reserved id reads as `never`, which would make the complement `any` and
        // the choice a lie.
        if !self.t.all_defined() {
            return None;
        }
        let c = {
            let n = self.t.negate(vid);
            let any = self.t.any();
            self.t.intersect(n, any)
        };
        let cd = self.t.data(c);
        if self.size(cd) >= self.size(vd) {
            return None;
        }
        let s = self.ty(c, P_NEGATE);
        Some((format!("!{s}"), P_NEGATE))
    }

    /// How many parts a positive rendering costs. A cofinite set and a full BDD each
    /// stand for infinitely many, so they cost more than anything finite can.
    fn size(&self, d: TyData) -> usize {
        const INF: usize = 1 << 16;
        let set = |id: AtomSetId| {
            let a = self.t.atomset_of(id);
            if a.neg { INF + a.names.len() } else { a.names.len() }
        };
        let bdd = |b: &super::bdd::Bdd, id| {
            b.paths(id)
                .iter()
                .map(|(pos, neg)| if pos.is_empty() { INF } else { pos.len() + neg.len() })
                .sum::<usize>()
        };
        d.base.count_ones() as usize
            + set(d.atoms)
            + set(d.vars)
            + bdd(&self.t.rec_bdd, d.records)
            + bdd(&self.t.tup_bdd, d.tuples)
            + bdd(&self.t.arrow_bdd, d.arrows)
    }

    fn positive(&mut self, d: TyData) -> Vec<(String, u8)> {
        let mut out = Vec::new();
        for (bit, name) in [
            (B_I64, "i64"),
            (B_F64, "f64"),
            (B_STR, "str"),
            (B_BOOL, "bool"),
            (B_NULL, "null"),
        ] {
            if d.base & bit != 0 {
                out.push((name.to_string(), P_ATOM));
            }
        }
        out.extend(self.names(d.atoms, ALL_ATOMS, true));
        out.extend(self.names(d.vars, ALL_VARS, false));
        let recs = self.t.rec_bdd.paths(d.records);
        out.extend(self.kind(recs, ALL_RECORDS, Printer::rec_atom));
        let tups = self.t.tup_bdd.paths(d.tuples);
        out.extend(self.kind(tups, ALL_TUPLES, Printer::tup_atom));
        let arrs = self.t.arrow_bdd.paths(d.arrows);
        out.extend(self.kind(arrs, ALL_ARROWS, Printer::arrow_atom));
        out
    }

    fn names(&self, id: AtomSetId, all: &str, colon: bool) -> Vec<(String, u8)> {
        let a = self.t.atomset_of(id);
        let one = |n: &NameId| {
            let s = self.t.name_str(*n);
            (if colon { format!(":{s}") } else { s.to_string() }, P_ATOM)
        };
        if !a.neg {
            return a.names.iter().map(one).collect();
        }
        // Cofinite. There is no supertype of every atom to write, so the set it is
        // taken from has to be named.
        let mut fs = vec![(all.to_string(), P_ATOM)];
        fs.extend(a.names.iter().map(|n| (format!("!{}", at(P_NEGATE, one(n))), P_NEGATE)));
        vec![intersect(fs)]
    }

    /// One BDD's DNF: a path is an intersection, the paths are a union.
    fn kind(
        &mut self,
        paths: Vec<(Vec<u32>, Vec<u32>)>,
        all: &str,
        atom: fn(&mut Self, u32) -> (String, u8),
    ) -> Vec<(String, u8)> {
        let mut out = Vec::new();
        for (pos, neg) in paths {
            let mut fs = Vec::new();
            // Every value of the kind, less the negatives: nothing positive names it.
            if pos.is_empty() {
                fs.push((all.to_string(), P_ATOM));
            }
            for i in pos {
                let f = atom(self, i);
                fs.push(f);
            }
            for j in neg {
                let f = atom(self, j);
                fs.push((format!("!{}", at(P_NEGATE, f)), P_NEGATE));
            }
            out.push(intersect(fs));
        }
        out
    }

    // ---- records ----

    fn rec_atom(&mut self, i: u32) -> (String, u8) {
        let a = self.t.rec_atoms[i as usize].clone();
        match self.nominal(&a) {
            Some(p) => p,
            None => self.structural(&a),
        }
    }

    /// `{#nominal: :Box, #0: i64}` is `Box[i64]`. Printing the encoding instead would
    /// be honest and useless.
    fn nominal(&mut self, a: &RecordAtom) -> Option<(String, u8)> {
        let tag = a.fields.iter().find(|f| f.0 == self.t.nominal_label)?.1;
        let name = self.singleton(tag)?;
        let args = self.args(a)?;
        let name = self.t.name_str(name).to_string();
        if args.is_empty() {
            return Some((name, P_ATOM));
        }
        let mut s = name;
        s.push('[');
        for (i, arg) in args.into_iter().enumerate() {
            if i > 0 {
                s.push_str(", ");
            }
            let r = self.ty(arg, P_ANY);
            s.push_str(&r);
        }
        s.push(']');
        Some((s, P_ATOM))
    }

    fn singleton(&self, t: TyId) -> Option<NameId> {
        let d = self.t.data(t);
        let a = self.t.atomset_of(d.atoms);
        let plain = d.base == 0
            && self.t.atomset_of(d.vars).is_empty_set()
            && d.records == super::bdd::FALSE
            && d.tuples == super::bdd::FALSE
            && d.arrows == super::bdd::FALSE;
        match (plain && !a.neg, a.names.as_slice()) {
            (true, [n]) => Some(*n),
            _ => None,
        }
    }

    /// `#0, #1, ...`, contiguous from zero. A gap means this is not an encoding this
    /// printer wrote, so it is not one it may claim to read.
    fn args(&self, a: &RecordAtom) -> Option<Vec<TyId>> {
        let mut args: Vec<(usize, TyId)> = Vec::new();
        for (l, t) in &a.fields {
            if let Some(i) = self.t.name_str(*l).strip_prefix('#').and_then(|s| s.parse().ok()) {
                args.push((i, *t));
            }
        }
        args.sort_by_key(|a| a.0);
        if args.iter().enumerate().any(|(i, a)| i != a.0) {
            return None;
        }
        Some(args.into_iter().map(|a| a.1).collect())
    }

    fn structural(&mut self, a: &RecordAtom) -> (String, u8) {
        let open = self.t.any_or_undef();
        let tag = self.struct_tag();
        let mut fields: Vec<(String, TyId)> = a
            .fields
            .iter()
            .filter(|(l, t)| !(*l == self.t.nominal_label && *t == tag))
            .map(|(l, t)| (self.t.name_str(*l).to_string(), *t))
            .collect();
        // Interning order is not reading order.
        fields.sort_by(|x, y| x.0.cmp(&y.0));

        let mut parts: Vec<String> = Vec::new();
        for (l, t) in fields {
            let r = self.ty(t, P_ANY);
            parts.push(format!("{l}: {r}"));
        }
        // An open record says nothing about the labels it does not name; anything
        // else constrains them, and dropping that would print a supertype as if it
        // were the type.
        if a.rest != open {
            let r = self.ty(a.rest, P_ANY);
            parts.push(format!("..: {r}"));
        }
        (format!("{{{}}}", parts.join(", ")), P_ATOM)
    }

    /// What `struct_ty` puts in `#nominal`: any tag, or none.
    fn struct_tag(&mut self) -> TyId {
        let all = self.t.any();
        let none = self.t.never();
        let atoms = self.t.data(all).atoms;
        let vars = self.t.data(none).vars;
        self.t.intern(TyData {
            base: B_UNDEF,
            atoms,
            vars,
            records: super::bdd::FALSE,
            tuples: super::bdd::FALSE,
            arrows: super::bdd::FALSE,
        })
    }

    // ---- tuples and arrows ----

    fn tup_atom(&mut self, i: u32) -> (String, u8) {
        let elems = self.t.tup_atoms[i as usize].elems.clone();
        // `(T)` is a grouping and `(T,)` is a parse error, so a one-tuple has no
        // surface form to print.
        if let [only] = elems.as_slice() {
            let r = self.ty(*only, P_ANY);
            return (format!("<tuple({r})>"), P_ATOM);
        }
        let mut s = String::from("(");
        for (n, e) in elems.into_iter().enumerate() {
            if n > 0 {
                s.push_str(", ");
            }
            let r = self.ty(e, P_ANY);
            s.push_str(&r);
        }
        s.push(')');
        (s, P_ATOM)
    }

    fn arrow_atom(&mut self, i: u32) -> (String, u8) {
        let a = self.t.arrow_atoms[i as usize].clone();
        let never = self.t.never();
        let mut s = String::from("(");
        for (n, p) in a.params.into_iter().enumerate() {
            if n > 0 {
                s.push_str(", ");
            }
            let r = self.ty(p, P_ANY);
            s.push_str(&r);
        }
        s.push(')');
        // An absent clause is `never`, and `throws never` is not how it is written.
        if a.throws != never {
            let r = self.ty(a.throws, P_UNION);
            let _ = write!(s, " throws {r}");
        }
        let r = self.ty(a.ret, P_ANY);
        let _ = write!(s, " -> {r}");
        (s, P_ANY)
    }
}


#[cfg(test)]
mod tests {
    use super::super::empty::Solver;
    use super::super::env::Env;
    use super::super::resolve::Scope;
    use super::*;
    use crate::{ast, lexer, parser};

    fn s() -> Solver {
        Solver::new()
    }

    fn p(s: &mut Solver, t: TyId) -> String {
        print(&mut s.t, t)
    }

    // ---- base, never, any ----

    #[test]
    fn primitives() {
        let mut s = s();
        for (t, want) in [
            (s.t.i64(), "i64"),
            (s.t.f64(), "f64"),
            (s.t.str(), "str"),
            (s.t.bool(), "bool"),
            (s.t.null(), "null"),
        ] {
            assert_eq!(p(&mut s, t), want);
        }
    }

    #[test]
    fn never_and_any_are_named_not_expanded() {
        let mut s = s();
        let n = s.t.never();
        let a = s.t.any();
        assert_eq!(p(&mut s, n), "never");
        assert_eq!(p(&mut s, a), "any");
    }

    #[test]
    fn a_union_of_bases() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        assert_eq!(p(&mut s, u), "i64 | str");
    }

    /// `B_UNDEF` is not a value and has no source syntax, so it is named rather than
    /// dropped: dropping it would print `{x: i64}` for a record whose `x` may be
    /// missing.
    #[test]
    fn the_absent_marker_is_named() {
        let mut s = s();
        let u = s.t.undef();
        assert_eq!(p(&mut s, u), "<absent>");

        let i = s.t.i64();
        let opt = s.t.union(i, u);
        assert_eq!(p(&mut s, opt), "i64 | <absent>");

        // Top of the field lattice, which is not `any`.
        let f = s.t.any_or_undef();
        assert_eq!(p(&mut s, f), "any | <absent>");
    }

    // ---- atoms and vars ----

    #[test]
    fn atoms_finite_and_cofinite() {
        let mut s = s();
        let ok = s.t.name("ok");
        let err = s.t.name("err");
        let a_ok = s.t.atom(ok);
        let a_err = s.t.atom(err);
        let u = s.t.union(a_ok, a_err);
        assert_eq!(p(&mut s, u), ":ok | :err");

        // A cofinite set prints as the negation it is, not as its infinite expansion.
        let not_ok = {
            let n = s.t.negate(a_ok);
            let a = s.t.any();
            s.t.intersect(n, a)
        };
        assert_eq!(p(&mut s, not_ok), "!:ok");

        let not_both = {
            let n = s.t.negate(u);
            let a = s.t.any();
            s.t.intersect(n, a)
        };
        assert_eq!(p(&mut s, not_both), "!(:ok | :err)");
    }

    /// Every atom but `:ok`, and nothing else — no primitive, no record. There is no
    /// supertype of the atoms to write, so the set has to be named.
    #[test]
    fn a_bare_cofinite_atom_set_says_so() {
        let mut s = s();
        let ok = s.t.name("ok");
        let a_ok = s.t.atom(ok);
        let every_atom = {
            let all = s.t.any();
            let none = s.t.never();
            let atoms = s.t.data(all).atoms;
            s.t.intern(TyData { atoms, ..s.t.data(none) })
        };
        let d = s.t.diff(every_atom, a_ok);
        assert_eq!(p(&mut s, d), "<atom> & !:ok");
    }

    /// Same bind, one kind up: the records BDD is full, and "every record" is not a
    /// type source can write either.
    #[test]
    fn a_full_kind_with_nothing_else_says_so() {
        let mut s = s();
        let i = s.t.i64();
        let every_record = {
            let none = s.t.never();
            let all = s.t.any();
            let records = s.t.data(all).records;
            s.t.intern(TyData { records, ..s.t.data(none) })
        };
        assert_eq!(p(&mut s, every_record), "<record>");

        let r = {
            let l = s.t.name("x");
            s.t.struct_ty(vec![(l, i)])
        };
        let d = s.t.diff(every_record, r);
        assert_eq!(p(&mut s, d), "<record> & !{x: i64}");
    }

    #[test]
    fn rigid_variables_print_their_name() {
        let mut s = s();
        let t = s.t.name("T");
        let v = s.t.var(t);
        assert_eq!(p(&mut s, v), "T");

        let nv = {
            let n = s.t.negate(v);
            let a = s.t.any();
            s.t.intersect(n, a)
        };
        assert_eq!(p(&mut s, nv), "!T");
    }

    // ---- records ----

    #[test]
    fn a_nominal_prints_its_name_not_its_encoding() {
        let mut s = s();
        let i = s.t.i64();
        let red = {
            let n = s.t.name("Red");
            let l = s.t.name("x");
            s.t.nominal(n, vec![], vec![(l, i)])
        };
        assert_eq!(p(&mut s, red), "Red");
    }

    #[test]
    fn generic_arguments_print_in_brackets() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        let b = {
            let n = s.t.name("Box");
            s.t.nominal(n, vec![u], vec![])
        };
        assert_eq!(p(&mut s, b), "Box[i64 | str]");

        let pair = {
            let n = s.t.name("Pair");
            s.t.nominal(n, vec![i, st], vec![])
        };
        assert_eq!(p(&mut s, pair), "Pair[i64, str]");
    }

    #[test]
    fn a_structural_record_prints_its_fields() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let r = {
            let name = s.t.name("name");
            let age = s.t.name("age");
            s.t.struct_ty(vec![(name, st), (age, i)])
        };
        assert_eq!(p(&mut s, r), "{age: i64, name: str}");
    }

    #[test]
    fn a_record_intersection_and_negation() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let a = {
            let l = s.t.name("a");
            s.t.struct_ty(vec![(l, i)])
        };
        let b = {
            let l = s.t.name("b");
            s.t.struct_ty(vec![(l, st)])
        };
        let both = s.t.intersect(a, b);
        assert_eq!(p(&mut s, both), "{a: i64} & {b: str}");

        let d = s.t.diff(a, b);
        assert_eq!(p(&mut s, d), "{a: i64} & !{b: str}");
    }

    // ---- tuples and arrows ----

    #[test]
    fn tuples() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let unit = s.t.tuple(vec![]);
        assert_eq!(p(&mut s, unit), "()");
        let t = s.t.tuple(vec![i, st]);
        assert_eq!(p(&mut s, t), "(i64, str)");
    }

    /// `(T)` is a grouping and `(T,)` is a parse error, so a one-tuple has no surface
    /// form. Printing `(i64)` would be a lie that parses.
    #[test]
    fn a_one_tuple_has_no_surface_form() {
        let mut s = s();
        let i = s.t.i64();
        let t = s.t.tuple(vec![i]);
        assert_eq!(p(&mut s, t), "<tuple(i64)>");
    }

    #[test]
    fn arrows_with_and_without_throws() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let nothrow = s.t.never();
        let f = s.t.arrow(vec![i], nothrow, st);
        assert_eq!(p(&mut s, f), "(i64) -> str", "an absent throws is not `throws never`");

        let e = {
            let n = s.t.name("err");
            s.t.atom(n)
        };
        let g = s.t.arrow(vec![i], e, st);
        assert_eq!(p(&mut s, g), "(i64) throws :err -> str");

        let h = s.t.arrow(vec![], nothrow, i);
        assert_eq!(p(&mut s, h), "() -> i64");
    }

    #[test]
    fn an_overload_is_an_intersection_of_parenthesised_arrows() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let nothrow = s.t.never();
        let a = s.t.arrow(vec![i], nothrow, i);
        let b = s.t.arrow(vec![st], nothrow, st);
        let f = s.t.intersect(a, b);
        assert_eq!(p(&mut s, f), "((i64) -> i64) & ((str) -> str)");
    }

    // ---- precedence ----

    #[test]
    fn precedence_matches_the_parser() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        let nothrow = s.t.never();

        // A union in a parameter needs no parens; an arrow in one does.
        let f = s.t.arrow(vec![u], nothrow, i);
        assert_eq!(p(&mut s, f), "(i64 | str) -> i64");
        let g = s.t.arrow(vec![f], nothrow, i);
        assert_eq!(p(&mut s, g), "((i64 | str) -> i64) -> i64");

        // `!` is tighter than `&`, which is tighter than `|`.
        let a = {
            let l = s.t.name("a");
            s.t.struct_ty(vec![(l, i)])
        };
        let b = {
            let l = s.t.name("b");
            s.t.struct_ty(vec![(l, st)])
        };
        let mixed = {
            let inter = s.t.intersect(a, b);
            s.t.union(inter, i)
        };
        assert_eq!(p(&mut s, mixed), "i64 | {a: i64} & {b: str}");
    }

    // ---- recursion ----

    /// `mu A = :ok | Box[A]`. The naive walk is infinite; the cycle is named where it
    /// closes.
    #[test]
    fn a_recursive_type_names_its_cycle() {
        let mut s = s();
        let a = s.t.reserve();
        let box_a = {
            let n = s.t.name("Box");
            s.t.nominal(n, vec![a], vec![])
        };
        let ok = s.t.name("ok");
        let a_ok = s.t.atom(ok);
        let body = s.t.union(a_ok, box_a);
        let d = s.t.data(body);
        s.t.define(a, d);

        assert_eq!(p(&mut s, a), "mu A0 = :ok | Box[A0]");
    }

    /// `mu F = null | (i64) -> F`
    #[test]
    fn recursion_through_an_arrow() {
        let mut s = s();
        let nothrow = s.t.never();
        let f = s.t.reserve();
        let i = s.t.i64();
        let arrow = s.t.arrow(vec![i], nothrow, f);
        let null = s.t.null();
        let body = s.t.union(null, arrow);
        let d = s.t.data(body);
        s.t.define(f, d);

        assert_eq!(p(&mut s, f), "mu A0 = null | ((i64) -> A0)");
    }

    /// `mu B = Box[B]` is empty, but it is not the `never` descriptor, and a printer
    /// does not get to decide emptiness. What it prints is what the type is.
    #[test]
    fn a_recursion_with_no_base_case_still_prints() {
        let mut s = s();
        let b = s.t.reserve();
        let box_b = {
            let n = s.t.name("Box");
            s.t.nominal(n, vec![b], vec![])
        };
        let d = s.t.data(box_b);
        s.t.define(b, d);
        assert_eq!(p(&mut s, b), "mu A0 = Box[A0]");
        assert!(s.is_empty(b));
    }

    /// `record Node { next: Node | null }`: the cycle closes through the field, and
    /// the nominal cuts it before it starts.
    #[test]
    fn a_recursive_nominal_prints_as_its_name() {
        let mut s = s();
        let n = s.t.reserve();
        let null = s.t.null();
        let next = s.t.union(n, null);
        let label = s.t.name("next");
        let nm = s.t.name("Node");
        let body = s.t.nominal(nm, vec![], vec![(label, next)]);
        let d = s.t.data(body);
        s.t.define(n, d);
        assert_eq!(p(&mut s, n), "Node");
        assert_eq!(p(&mut s, next), "null | Node");
    }

    // ---- the declared name wins ----

    #[test]
    fn a_declared_name_is_preferred_to_the_expansion() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();
        let u = s.t.union(i, st);
        let n = s.t.name("Scalar");
        s.t.defs.insert(n, u);
        assert_eq!(p(&mut s, u), "Scalar");

        // And inside a constructor, which is where it earns its keep.
        let b = {
            let bn = s.t.name("Box");
            s.t.nominal(bn, vec![u], vec![])
        };
        assert_eq!(p(&mut s, b), "Box[Scalar]");
    }

    // ---- print -> parse -> resolve ----

    fn parse(src: &str) -> ast::Module {
        let tokens = lexer::lex(src).expect("the fixture lexes");
        let (m, errs) = parser::parse(&tokens, src.len());
        assert!(errs.is_empty(), "parse errors in {src:?}: {errs:?}");
        m.expect("the fixture parses")
    }

    fn env(src: &str) -> Env {
        Env::build(&parse(src))
    }

    fn ty(e: &mut Env, src: &str) -> TyId {
        let m = parse(&format!("fn probe(x: {src}) {{ }}"));
        let ast::DeclKind::Fn(f) = &m.decls[0].kind else { unreachable!("the fixture is a fn") };
        let scope = Scope::new(&[]);
        e.resolve(&scope, &f.params[0].ty)
    }

    /// The property: printing does not change the type. Hash-consing makes the
    /// re-resolved id comparable, so `is_equiv` is cheap and exact.
    fn round_trip(e: &mut Env, src: &str) {
        let t = ty(e, src);
        let printed = print(&mut e.solver.t, t);
        let back = ty(e, &printed);
        assert!(
            e.solver.is_equiv(t, back),
            "{src:?} printed as {printed:?}, which is a different type"
        );
        assert!(e.errors().is_empty(), "{printed:?} did not re-resolve cleanly");
    }

    #[test]
    fn source_types_round_trip() {
        let mut e = env("record Red { x: i64 }\nrecord Box[T] { v: T }\nrecord Pair[A, B] { a: A, b: B }");
        for src in [
            "i64",
            "f64",
            "str",
            "bool",
            "null",
            "any",
            "i64 | str",
            "i64 | null",
            ":ok",
            ":ok | :err",
            ":ok | :err | str",
            "!:ok",
            "!(:ok | :err)",
            "!i64",
            "Red",
            "Box[i64]",
            "Box[i64 | str]",
            "Pair[i64, str]",
            "Box[Box[i64]]",
            "{ x: i64 }",
            "{ x: i64, y: str }",
            "{ x: i64 } & { y: str }",
            "!{ x: i64 }",
            "(i64, str)",
            "(i64, str, bool)",
            "()",
            "(i64) -> str",
            "() -> i64",
            "(i64, str) -> bool",
            "(i64) throws :err -> str",
            "((i64) -> i64) -> i64",
            "(i64) -> ((i64) -> i64)",
            "Box[(i64) -> i64]",
            "((i64) -> i64) & ((str) -> str)",
            "i64 | ({ x: i64 } & { y: str })",
            "Red | :ok | i64",
        ] {
            round_trip(&mut e, src);
        }
    }

    /// The gaps, stated rather than hidden. Each of these prints something true that
    /// source cannot say, so it names the type without re-parsing to it.
    #[test]
    fn what_does_not_round_trip() {
        let mut s = s();
        let i = s.t.i64();
        let st = s.t.str();

        // `never` is not a writable type. The design's word for ⊥ beats an expansion
        // that does not exist.
        let n = s.t.never();
        assert_eq!(p(&mut s, n), "never");

        // The field lattice's top, and the marker in it.
        let f = s.t.any_or_undef();
        assert_eq!(p(&mut s, f), "any | <absent>");

        // A one-tuple.
        let one = s.t.tuple(vec![i]);
        assert_eq!(p(&mut s, one), "<tuple(i64)>");

        // A μ with no declaration to name it.
        let a = s.t.reserve();
        let box_a = {
            let n = s.t.name("Box");
            s.t.nominal(n, vec![a], vec![])
        };
        let u = s.t.union(st, box_a);
        let d = s.t.data(u);
        s.t.define(a, d);
        assert_eq!(p(&mut s, a), "mu A0 = str | Box[A0]");
    }

    // ---- the printer never hangs ----

    #[test]
    fn a_deep_type_is_cut_rather_than_overflowing_the_stack() {
        let mut s = s();
        let mut t = s.t.i64();
        for _ in 0..200 {
            let n = s.t.name("Box");
            t = s.t.nominal(n, vec![t], vec![]);
        }
        let out = p(&mut s, t);
        assert!(out.contains(CUT), "a type deeper than the cap is truncated: {out}");
    }
}
