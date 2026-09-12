use slew::ast::*;
use slew::parser::parse;

fn ok(src: &str) -> Block {
    parse(src.as_bytes()).unwrap_or_else(|e| panic!("{e}\nsource: {src}"))
}

fn fails(src: &str) {
    assert!(
        parse(src.as_bytes()).is_err(),
        "expected parse error: {src}"
    );
}

#[test]
fn statements() {
    ok(";;;");
    ok("local a, b <const>, c <close> = 1, 2");
    ok("a, b.c, d[1] = f(), 2, 3");
    ok("do local x = 1 end");
    ok("while a < 10 do a = a + 1 end");
    ok("repeat a = a + 1 until a > 10");
    ok("if a then b() elseif c then d() elseif e then f() else g() end");
    ok("for i = 1, 10 do end for i = 10, 1, -1 do end");
    ok("for k, v in pairs(t) do print(k, v) end");
    ok("function a.b.c:d(x, y, ...) return x end");
    ok("local function fib(n) if n < 2 then return n end return fib(n-1) + fib(n-2) end");
    ok("goto continue ::continue:: break");
    ok("return");
    ok("return 1, 2, f()");
    ok("return;");
}

#[test]
fn expressions() {
    ok("x = nil or false or true and 1");
    ok("x = -2 ^ 2"); // -(2^2)
    ok("x = a .. b .. c");
    ok("x = 1 + 2 * 3 - 4 / 5 // 6 % 7");
    ok("x = a < b or a > c or a <= d or a >= e or a ~= f or a == g");
    ok("x = a & b | c ~ d << e >> f");
    ok("x = ~a + -b + not c + #d");
    ok("x = f()(g())[h()].i:j(k)");
    ok("x = f 'string arg'");
    ok("x = f {1, 'two', three = 3}");
    ok("x = obj:method 'arg'");
    ok("x = (f())");
    ok("x = ...");
    ok("x = function(a, b) return a + b end");
}

#[test]
fn table_constructors() {
    ok("t = {}");
    ok("t = {1, 2, 3}");
    ok("t = {1, 2, 3,}");
    ok("t = {a = 1, b = 2; [k] = v, 10}");
    ok("t = {f()}");
    ok("t = {nested = {1, {2, {3}}}}");
}

#[test]
fn operator_precedence_shape() {
    // 1 + 2 * 3 parses as 1 + (2 * 3)
    let b = ok("x = 1 + 2 * 3");
    let Stmt::Assign { values, .. } = &b.stmts[0] else {
        panic!()
    };
    let Expr::BinOp {
        op: BinOp::Add,
        rhs,
        ..
    } = &values[0]
    else {
        panic!("expected Add at root")
    };
    assert!(matches!(**rhs, Expr::BinOp { op: BinOp::Mul, .. }));

    // a .. b .. c is right-associative: a .. (b .. c)
    let b = ok("x = a .. b .. c");
    let Stmt::Assign { values, .. } = &b.stmts[0] else {
        panic!()
    };
    let Expr::BinOp {
        op: BinOp::Concat,
        rhs,
        ..
    } = &values[0]
    else {
        panic!()
    };
    assert!(matches!(
        **rhs,
        Expr::BinOp {
            op: BinOp::Concat,
            ..
        }
    ));

    // -2 ^ 2 is -(2 ^ 2)
    let b = ok("x = -2 ^ 2");
    let Stmt::Assign { values, .. } = &b.stmts[0] else {
        panic!()
    };
    assert!(matches!(&values[0], Expr::UnOp { op: UnOp::Neg, .. }));
}

#[test]
fn method_sugar() {
    // function a:m() end gets implicit self
    let b = ok("function a:m(x) end");
    let Stmt::Function { body, .. } = &b.stmts[0] else {
        panic!()
    };
    assert_eq!(&*body.params[0], "self");
    assert_eq!(&*body.params[1], "x");
}

#[test]
fn syntax_errors() {
    fails("x =");
    fails("if a then");
    fails("for i = 1 do end");
    fails("local 1 = 2");
    fails("f() = 3");
    fails("x = y z"); // two expression statements that aren't calls
    fails("return return");
    fails("local x <unknown> = 1");
    fails("a.b"); // not a statement
    fails("end");
}

#[test]
fn repeat_until_scoping_shape() {
    // until condition can mention locals from the body — must parse
    ok("repeat local done = check() until done");
}
