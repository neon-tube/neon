use super::print;
use super::*;
use crate::ir::repr::Repr;
use crate::typecheck::types::Types;

#[test]
fn a_small_function_builds_and_prints() {
    let mut ty = Types::new();
    let i64ty = ty.i64();

    // fn add5(x: i64) -> i64 { x + 5 }
    let mut b = Builder::new("add5", Repr::I64);
    let x = b.block_param(BlockId(0), Repr::I64, i64ty);
    let c = b.emit(Op::ConstI64(5), Repr::I64, i64ty);
    let sum = b.emit(Op::Prim(PrimOp::Add, vec![x, c]), Repr::I64, i64ty);
    b.terminate(Term::Ret(Some(sum)));
    let f = b.finish(vec![x]);

    let printed = print::func(&f);
    assert_eq!(
        printed,
        "\
fn @add5(%0 i64) -> i64 {
  block0:
    %1 = const.i64 5
    %2 = prim.add %0, %1
    ret %2
}
"
    );
}

#[test]
fn control_flow_uses_block_arguments() {
    let mut ty = Types::new();
    let i64ty = ty.i64();
    let boolty = ty.bool();

    // fn pick(c: bool) -> i64 { if c { 1 } else { 2 } }
    let mut b = Builder::new("pick", Repr::I64);
    let c = b.block_param(BlockId(0), Repr::Bool, boolty);
    let then_b = b.new_block();
    let else_b = b.new_block();
    let join = b.new_block();
    let result = b.block_param(join, Repr::I64, i64ty);

    b.switch_to(BlockId(0));
    b.terminate(Term::Branch {
        cond: c,
        then: Target {
            to: then_b,
            args: vec![],
        },
        els: Target {
            to: else_b,
            args: vec![],
        },
    });

    b.switch_to(then_b);
    let one = b.emit(Op::ConstI64(1), Repr::I64, i64ty);
    b.terminate(Term::Jump(Target {
        to: join,
        args: vec![one],
    }));

    b.switch_to(else_b);
    let two = b.emit(Op::ConstI64(2), Repr::I64, i64ty);
    b.terminate(Term::Jump(Target {
        to: join,
        args: vec![two],
    }));

    b.switch_to(join);
    b.terminate(Term::Ret(Some(result)));

    let f = b.finish(vec![c]);
    let printed = print::func(&f);
    assert_eq!(
        printed,
        "\
fn @pick(%0 bool) -> i64 {
  block0:
    branch %0, block1, block2
  block1:
    %2 = const.i64 1
    jump block3(%2)
  block2:
    %3 = const.i64 2
    jump block3(%3)
  block3(%1 i64):
    ret %1
}
"
    );
}

#[test]
fn reprs_print_in_their_own_syntax() {
    assert_eq!(print::repr(&Repr::List(Box::new(Repr::I64))), "list[i64]");
    assert_eq!(
        print::repr(&Repr::Map(Box::new(Repr::Str), Box::new(Repr::I64))),
        "map[str, i64]"
    );
    assert_eq!(print::repr(&Repr::Nullable(Box::new(Repr::Str))), "str?");
    assert_eq!(
        print::repr(&Repr::Record {
            name: Some("Circle".into()),
            fields: vec![("r".into(), Repr::I64)],
        }),
        "Circle{r: i64}"
    );
    assert_eq!(
        print::repr(&Repr::Closure {
            params: vec![Repr::I64],
            throws: Box::new(Repr::Never),
            ret: Box::new(Repr::Str),
        }),
        "fn(i64) -> str"
    );
    assert_eq!(
        print::repr(&Repr::Closure {
            params: vec![Repr::I64],
            throws: Box::new(Repr::Tag),
            ret: Box::new(Repr::Str),
        }),
        "fn(i64) throws tag -> str"
    );
    assert_eq!(
        print::repr(&Repr::Union(vec![Repr::I64, Repr::Str])),
        "i64 | str"
    );
}
