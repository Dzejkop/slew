//! Compile-time `goto`/label checking and `<const>`/`<close>` validation,
//! mirroring the substrings the upstream `goto.lua`, `errors.lua` and
//! `constructs.lua` files match on.

use slew::{Lua, Step};

/// Compiles `src`, expecting a compile/parse failure, and returns its message.
fn load_err(src: &str) -> String {
    let mut lua = Lua::new();
    lua.load(src)
        .err()
        .unwrap_or_else(|| panic!("expected a load error for:\n{src}"))
        .to_string()
}

/// Runs `src` to completion and returns the values rendered via `tostring`.
fn eval_multi(src: &str) -> Vec<String> {
    let mut lua = Lua::new();
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..1000 {
        match exec.step(&mut lua, 100_000) {
            Ok(Step::Done(vals)) => {
                return vals.iter().map(|v| lua.display_value(*v)).collect();
            }
            Ok(Step::Pending) => {}
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish:\n{src}");
}

fn eval(src: &str) -> String {
    let mut vals = eval_multi(src);
    assert_eq!(vals.len(), 1, "expected 1 return value from:\n{src}");
    vals.pop().unwrap()
}

/// Runs `src`, asserting it compiles and completes without error.
fn run_ok(src: &str) {
    let _ = eval_multi(src);
}

// ---- label visibility & repeated/undefined labels ----

#[test]
fn label_inside_block_is_invisible() {
    assert!(load_err("goto l1; do ::l1:: end").contains("label 'l1'"));
    assert!(load_err("do ::l1:: end goto l1;").contains("label 'l1'"));
    assert!(load_err("do ::l1:: end goto l1").contains("label 'l1'"));
    assert!(load_err("goto l1 do ::l1:: end").contains("label 'l1'"));
}

#[test]
fn repeated_labels_report_the_original_line() {
    assert!(load_err("::l1:: ::l1::").contains("label 'l1' already defined on line 1"));
    assert!(load_err("::l1:: do ::l1:: end").contains("label 'l1' already defined on line 1"));
    // A label may be reused in a sibling block or a different function.
    run_ok("do ::l1:: end do ::l1:: end");
    run_ok("local function f() ::l:: end local function g() ::l:: end");
}

#[test]
fn undefined_goto_names_the_label() {
    let err = load_err("goto nope");
    assert!(
        err.contains("no visible label 'nope' for <goto> at line 1"),
        "{err}"
    );
}

#[test]
fn break_outside_a_loop_is_rejected() {
    assert!(load_err("break").contains("break outside loop at line 1"));
    assert!(load_err("do break end").contains("break outside loop at line 1"));
    // Inside a loop it is fine.
    run_ok("local n = 0 for i = 1, 10 do n = n + 1 if i == 3 then break end end");
}

// ---- jumping over locals ----

#[test]
fn goto_may_not_jump_over_a_local() {
    assert!(load_err("goto l1; local aa ::l1:: ::l2:: print(3)").contains("local 'aa'"));
    let err = load_err("do local bb, cc; goto l1; end\nlocal aa\n::l1:: print(3)");
    assert!(err.contains("local 'aa'"), "{err}");
    // The reported name is the first local active at the label.
    let err = load_err("goto l1\nlocal a, b, c\n::l1:: print(1)");
    assert!(err.contains("local 'a'"), "{err}");
}

#[test]
fn goto_may_jump_over_a_local_to_end_of_block() {
    assert_eq!(eval("do goto l1 local a = 23 ::l1:: ; end return 1"), "1");
    // A repeat's condition still sees the body's locals, so a trailing label
    // is *not* at the end of the block there.
    let err = load_err(
        "repeat\n  if x then goto cont end\n  local xuxu = 10\n  ::cont::\nuntil xuxu < x",
    );
    assert!(err.contains("local 'xuxu'"), "{err}");
}

#[test]
fn backward_goto_out_of_scopes_works() {
    // `goto` backwards out of a nested scope, re-declaring `y` on the way.
    assert_eq!(
        eval(
            "local x\n::L1::\nlocal y\nif x == nil then x = 1; goto L1 else x = x + 1 end\n\
             return x"
        ),
        "2"
    );
    // Simple forward/backward cycle within a block.
    assert_eq!(
        eval(
            "local x = 0\ndo local y = 12 goto l1\n::l2:: x = x + 1; goto l3\n\
             ::l1:: x = y; goto l2\nend\n::l3:: return x"
        ),
        "13"
    );
}

// ---- attributes: <const> / <close> ----

#[test]
fn unknown_attribute_is_rejected() {
    let err = load_err("local x <XXX> = 10");
    assert!(err.contains("unknown attribute 'XXX'"), "{err}");
}

#[test]
fn assigning_to_const_is_rejected() {
    let err = load_err("local xxx <const> = 20; xxx = 10");
    assert!(
        err.contains("attempt to assign to const variable 'xxx'"),
        "{err}"
    );
}

#[test]
fn assigning_to_const_through_an_upvalue_is_rejected() {
    let src = "\
local xx;
local xxx <const> = 20;
local yyy;
local function foo ()
  local abc = xx + yyy + xxx;
  return function () return function () xxx = yyy end end
end
";
    let err = load_err(src);
    assert!(
        err.contains("attempt to assign to const variable 'xxx'"),
        "{err}"
    );
    // A plain (non-const) captured local stays writable.
    run_ok("local x = 1 local function f() x = 2 end f()");
}

#[test]
fn close_variables_are_read_only() {
    let err = load_err("local x <close> = nil\nx = 5");
    assert!(
        err.contains("attempt to assign to const variable 'x'"),
        "{err}"
    );
    // Only one to-be-closed variable per declaration.
    let err = load_err("local a <close>, b <close> = nil, nil");
    assert!(
        err.contains("multiple to-be-closed variables in local list"),
        "{err}"
    );
}

#[test]
fn const_attribute_applies_per_name() {
    // The attribute binds to its own name only.
    assert_eq!(eval("local a, b <const> = 1, 2 return a + b"), "3");
    assert_eq!(eval("local a <const>, b = 1, 2 b = 3 return a + b"), "4");
    let err = load_err("local a, b <const> = 1, 2 b = 3");
    assert!(
        err.contains("attempt to assign to const variable 'b'"),
        "{err}"
    );
}

#[test]
fn close_variables_run_their_close_handler_on_scope_exit() {
    assert_eq!(
        eval(
            "local closed = false\n\
             do\n\
               local x <close> = setmetatable({}, {__close = function() closed = true end})\n\
             end\n\
             return closed"
        ),
        "true"
    );
}

#[test]
fn goto_out_of_a_close_scope_closes_it() {
    assert_eq!(
        eval(
            "local closed = false\n\
             do\n\
               local x <close> = setmetatable({}, {__close = function() closed = true end})\n\
               goto done\n\
             end\n\
             ::done::\n\
             return closed"
        ),
        "true"
    );
}
