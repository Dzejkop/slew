use slew::ast::*;
use slew::parser::parse;
use test_case::test_case;

fn ok(src: &str) -> Block {
    parse(src.as_bytes()).unwrap_or_else(|e| panic!("{e}\nsource: {src}"))
}

fn fails(src: &str) {
    assert!(
        parse(src.as_bytes()).is_err(),
        "expected parse error: {src}"
    );
}

#[test_case(";;;"; "semicolons_only")]
#[test_case("local a, b <const>, c <close> = 1, 2"; "attributes_and_locals")]
#[test_case("a, b.c, d[1] = f(), 2, 3"; "multi_assignment")]
#[test_case("do local x = 1 end"; "do_block")]
#[test_case("while a < 10 do a = a + 1 end"; "while_loop")]
#[test_case("repeat a = a + 1 until a > 10"; "repeat_loop")]
#[test_case("if a then b() elseif c then d() elseif e then f() else g() end"; "if_elseif_else")]
#[test_case("for i = 1, 10 do end for i = 10, 1, -1 do end"; "numeric_for")]
#[test_case("for k, v in pairs(t) do print(k, v) end"; "generic_for")]
#[test_case("function a.b.c:d(x, y, ...) return x end"; "function_sugar")]
#[test_case("local function fib(n) if n < 2 then return n end return fib(n-1) + fib(n-2) end"; "local_function")]
#[test_case("goto continue ::continue:: break"; "goto_and_label")]
#[test_case("return"; "bare_return")]
#[test_case("return 1, 2, f()"; "return_values")]
#[test_case("return;"; "return_with_semicolon")]
fn statements(src: &str) {
    ok(src);
}

#[test_case("x = nil or false or true and 1"; "logical_ops")]
#[test_case("x = -2 ^ 2"; "unary_minus_binds_looser_than_power")]
#[test_case("x = a .. b .. c"; "concat")]
#[test_case("x = 1 + 2 * 3 - 4 / 5 // 6 % 7"; "arithmetic")]
#[test_case("x = a < b or a > c or a <= d or a >= e or a ~= f or a == g"; "comparisons")]
#[test_case("x = a & b | c ~ d << e >> f"; "bitwise")]
#[test_case("x = ~a + -b + not c + #d"; "unary_ops")]
#[test_case("x = f()(g())[h()].i:j(k)"; "call_chain")]
#[test_case("x = f 'string arg'"; "string_call_arg")]
#[test_case("x = f {1, 'two', three = 3}"; "table_call_arg")]
#[test_case("x = obj:method 'arg'"; "method_call_arg")]
#[test_case("x = (f())"; "paren_expr")]
#[test_case("x = ..."; "vararg")]
#[test_case("x = function(a, b) return a + b end"; "anonymous_function")]
fn expressions(src: &str) {
    ok(src);
}

#[test_case("t = {}"; "empty_table")]
#[test_case("t = {1, 2, 3}"; "array_table")]
#[test_case("t = {1, 2, 3,}"; "trailing_comma")]
#[test_case("t = {a = 1, b = 2; [k] = v, 10}"; "mixed_keys")]
#[test_case("t = {f()}"; "call_in_table")]
#[test_case("t = {nested = {1, {2, {3}}}}"; "nested_tables")]
fn table_constructors(src: &str) {
    ok(src);
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

#[test_case("x ="; "missing_rhs")]
#[test_case("if a then"; "unterminated_if")]
#[test_case("for i = 1 do end"; "numeric_for_missing_limit")]
#[test_case("local 1 = 2"; "bad_local_name")]
#[test_case("f() = 3"; "assign_to_call")]
#[test_case("x = y z"; "adjacent_expressions")]
#[test_case("return return"; "return_return")]
#[test_case("local x <unknown> = 1"; "unknown_attribute")]
#[test_case("a.b"; "non_statement_expression")]
#[test_case("end"; "stray_end")]
fn syntax_errors(src: &str) {
    fails(src);
}

#[test]
fn repeat_until_scoping_shape() {
    // until condition can mention locals from the body — must parse
    ok("repeat local done = check() until done");
}
