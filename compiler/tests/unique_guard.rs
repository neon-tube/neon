//! Pins for the sole-ownership analysis (`ir::unique`): the shapes that must never
//! qualify, and the one that must.
//!
//! Every negative here is a miscompile if it ever qualifies — being wrong does not
//! crash, it silently mutates a list somebody else can observe — so each test names the
//! rule it pins. The semantic counterparts live in `tests/lang/collections/` (the
//! caller-alias, read-after-write, and caught-error programs), where the *output* is the
//! oracle; these pin the analysis's answer directly so a regression is caught at the
//! query rather than three passes later.

use neon_compiler::ir::ssa::Op;
use neon_compiler::ir::{self, unique, Stage};
use neon_compiler::typecheck::env::Unit;
use neon_compiler::typecheck::{check::check_all, Env};
use neon_compiler::{lexer, parser};
use std::path::Path;

fn stdlib_modules() -> (Vec<(Vec<String>, neon_compiler::ast::Module)>, u32) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../stdlib");
    fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        for e in std::fs::read_dir(dir).expect("readable") {
            let p = e.expect("entry").path();
            if p.is_dir() {
                collect(root, &p, out);
            } else if p.extension().is_some_and(|x| x == "neon") {
                let rel = p
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, std::fs::read_to_string(&p).expect("readable")));
            }
        }
    }
    let mut sources = Vec::new();
    collect(&root, &root, &mut sources);
    neon_compiler::stdlib::parse_from(&sources, 0).expect("stdlib parses")
}

/// Compile a source string through the real pipeline to the requested stage.
fn compile(src: &str, stage: Stage) -> ir::ssa::Program {
    let (std_owned, next_id) = stdlib_modules();
    let tokens = lexer::lex(src).expect("lexes");
    let (module, perrs) = parser::parse(&tokens, src.len());
    assert!(perrs.is_empty(), "parses: {perrs:?}");
    let mut module = module.expect("a module");
    neon_compiler::ast::number_exprs_from(&mut module, next_id);
    let mut modules: Vec<(Vec<String>, &_)> =
        std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
    modules.push((Vec::new(), &module));
    let mut env = Env::build_with(&modules, Unit::RootApplication);
    assert!(env.errors().is_empty(), "resolves: {:?}", env.errors());
    let (result, errs) = check_all(&mut env, &modules);
    assert!(errs.is_empty(), "checks: {errs:?}");
    let libs: Vec<(Vec<String>, &_)> = std_owned.iter().map(|(p, m)| (p.clone(), m)).collect();
    ir::compile(&env, &result, &module, &libs, stage)
}

/// The candidates the analysis reports for one function of a program, asked — as the
/// pipeline asks — at `Stage::Optimised`, before refcounting muddies ownership.
fn candidates_for(src: &str, func: &str) -> Vec<unique::Candidate> {
    let program = compile(src, Stage::Optimised);
    unique::candidates(&program)
        .into_iter()
        .filter(|c| c.func == func)
        .collect()
}

/// Whether the finished pipeline (`Stage::Final`, after `unique::apply`) left the given
/// native in the named function.
fn final_has_native(src: &str, func: &str, symbol: &str) -> bool {
    let program = compile(src, Stage::Final);
    let f = program
        .funcs
        .iter()
        .find(|f| f.name == func)
        .expect("function exists");
    f.blocks
        .iter()
        .flat_map(|b| &b.insts)
        .any(|i| matches!(&i.op, Op::Native { symbol: s, .. } if s == symbol))
}

const QUALIFIES: &str = r##"
use std::io;
use std::collections::list;

fn bump(xs: List[i64], n: i64) -> List[i64] {
    let acc = xs;
    let i = 0;
    while i < n {
        acc = try! list::set(acc, 0, acc[0] + 1);
        i = i + 1;
    };
    acc
}

fn main() {
    io::println("#{bump(list::repeat(0, 3), 2)[0]}");
}
"##;

#[test]
fn a_scalar_loop_write_qualifies_and_is_rewritten() {
    let found = candidates_for(QUALIFIES, "bump");
    assert_eq!(found.len(), 1, "one chain: {found:?}");
    assert_eq!(found[0].writes, 1);
    assert!(found[0].scalar);
    // And the pipeline acts on it: the write is in place, uniqueness is established.
    assert!(final_has_native(QUALIFIES, "bump", "neon_list_set_inplace"));
    assert!(final_has_native(
        QUALIFIES,
        "bump",
        "neon_list_ensure_unique"
    ));
}

#[test]
fn an_escape_into_a_container_disqualifies() {
    // Each iteration's snapshot goes into `lists`, so a second reference outlives the
    // write and the next iteration must clone. (The escape has to be one the optimiser
    // cannot dissolve: a *call* to a trivial function inlines to a read and rightly
    // qualifies — which is how the first version of this test refuted itself.)
    let src = r##"
use std::io;
use std::collections::list;

fn f(xs: List[i64], n: i64) -> List[List[i64]] {
    let acc = xs;
    let lists: List[List[i64]] = [];
    let i = 0;
    while i < n {
        acc = try! list::set(acc, 0, i);
        lists = list::push(lists, acc);
        i = i + 1;
    };
    lists
}

fn main() {
    io::println("#{list::len(f(list::repeat(0, 2), 2))}");
}
"##;
    assert!(candidates_for(src, "f").is_empty());
    assert!(!final_has_native(src, "f", "neon_list_set_inplace"));
}

#[test]
fn an_escape_behind_a_join_disqualifies() {
    // The whole-chain rule. The escape takes the *join parameter* after the `if`, not
    // the write's result directly — the shape the first version of the walk, which
    // stopped at the first non-header carry, would have missed.
    let src = r##"
use std::io;
use std::collections::list;

fn f(xs: List[i64], n: i64) -> List[List[i64]] {
    let acc = xs;
    let lists: List[List[i64]] = [];
    let i = 0;
    while i < n {
        acc = try! list::set(acc, 0, i);
        if i == 0 {
            io::println("first");
        } else {
            io::println("later");
        };
        lists = list::push(lists, acc);
        i = i + 1;
    };
    lists
}

fn main() {
    io::println("#{list::len(f(list::repeat(0, 2), 2))}");
}
"##;
    assert!(candidates_for(src, "f").is_empty());
    assert!(!final_has_native(src, "f", "neon_list_set_inplace"));
}

#[test]
fn a_read_after_the_write_disqualifies() {
    // The order rule: `acc` is read after the write that consumed it and must keep
    // showing the old contents. Semantic twin:
    // `tests/lang/collections/a_read_after_a_loop_write_sees_the_old_list.neon`.
    let src = r##"
use std::io;
use std::collections::list;

fn main() {
    let acc = list::repeat(0, 3);
    let i = 0;
    while i < 3 {
        let next = try! list::set(acc, i, i + 1);
        io::println("#{acc[i]} -> #{next[i]}");
        acc = next;
        i = i + 1;
    };
    io::println("#{acc[0]}");
}
"##;
    assert!(candidates_for(src, "main").is_empty());
    assert!(!final_has_native(src, "main", "neon_list_set_inplace"));
}

#[test]
fn a_forked_write_disqualifies() {
    // Two writes consuming one value are two logical lists that must not share a buffer.
    let src = r##"
use std::io;
use std::collections::list;

fn main() {
    let acc = list::repeat(0, 3);
    let total = 0;
    let i = 0;
    while i < 3 {
        let b = try! list::set(acc, 0, i);
        let c = try! list::set(acc, 1, i);
        total = total + b[0];
        acc = c;
        i = i + 1;
    };
    io::println("#{total} #{acc[0]} #{acc[1]}");
}
"##;
    assert!(candidates_for(src, "main").is_empty());
    assert!(!final_has_native(src, "main", "neon_list_set_inplace"));
}

#[test]
fn a_counted_element_is_declined() {
    // Sole-owned, and reported as such — but `neon_list_set_scalar_inplace` is a raw
    // store, so a refcounted element's displaced value would leak. Declined, not
    // rewritten. Semantic twin:
    // `tests/lang/collections/list_set_releases_a_replaced_element.neon`.
    let src = r##"
use std::io;
use std::collections::list;

fn g(n: i64) -> List[str] {
    let acc = list::repeat("x", 3);
    let i = 0;
    while i < n {
        acc = try! list::set(acc, 0, "#{i}");
        i = i + 1;
    };
    acc
}

fn main() {
    io::println(g(2)[0]);
}
"##;
    let found = candidates_for(src, "g");
    assert_eq!(found.len(), 1, "sole-owned, so reported: {found:?}");
    assert!(!found[0].scalar, "but not scalar");
    assert!(!final_has_native(src, "g", "neon_list_set_inplace"));
}

#[test]
fn a_caught_error_disqualifies() {
    // The in-place primitive traps where `try!` panics; a `catch` observes the
    // difference, so the write keeps the generic call. Semantic twin:
    // `tests/lang/collections/a_caught_write_error_survives_a_loop.neon`.
    let src = r##"
use std::io;
use std::collections::list;

fn main() {
    let acc = list::repeat(0, 3);
    let i = 0;
    while i < 5 {
        acc = try list::set(acc, i, i) catch (e) {
            io::println("caught");
            acc
        };
        i = i + 1;
    };
    io::println("#{acc[0]}");
}
"##;
    assert!(candidates_for(src, "main").is_empty());
    assert!(!final_has_native(src, "main", "neon_list_set_inplace"));
}
