//! Phase 7: the introspective `debug` library (getinfo/traceback/upvalues/
//! metatables), implemented as VM intrinsics.

use slew::{Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..10_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(vals)) => return vals,
            Ok(Step::Pending) => continue,
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish: {src}");
}

fn eval(src: &str) -> String {
    let mut lua = Lua::new();
    let vals = run(&mut lua, src);
    assert!(!vals.is_empty(), "expected a return value from: {src}");
    lua.display_value(vals[0])
}

/// Runs `src` (which should assert internally) and checks it returns true.
fn ok(src: &str) -> bool {
    let mut lua = Lua::new();
    let vals = run(&mut lua, src);
    matches!(vals.first(), Some(Value::Bool(true)))
}

// ---- getinfo ----

#[test]
fn getinfo_function_fields() {
    // `f` is defined on line 1 (`local f = function () end`); main chunk is
    // linedefined 0 with what == "main".
    let src = "\
local f = function (a, b, ...) return a end
local i = debug.getinfo(f)
assert(i.what == 'Lua', i.what)
assert(i.linedefined == 1, i.linedefined)
assert(i.lastlinedefined == 1, i.lastlinedefined)
assert(i.nparams == 2, i.nparams)
assert(i.isvararg == true)
assert(i.nups == 0)
assert(i.func == f)
assert(i.currentline == -1)
assert(type(i.source) == 'string')
local m = debug.getinfo(1)
assert(m.what == 'main', m.what)
assert(debug.getinfo(f, 'L').activelines ~= nil)
return true
";
    assert!(ok(src));
}

#[test]
fn getinfo_currentline_is_accurate() {
    // Call sits on line 2 of this chunk.
    let src =
        "local function where ()\n  return debug.getinfo(1, 'l').currentline\nend\nreturn where()";
    assert_eq!(eval(src), "2");
    // Newlines inside string literals shift the line count of later code,
    // exactly as the upstream `literals.lua` `lexstring` helper relies on.
    let src = concat!(
        "local debug = require'debug'\n",
        "local function lexstring (x, n)\n",
        "  local f = assert(load('return ' .. x .. ', require\"debug\".getinfo(1).currentline', ''))\n",
        "  local s, l = f()\n",
        "  assert(l == n, l .. ' ~= ' .. n)\n",
        "end\n",
        "lexstring(\"'abc\\\\z  \\n   efg'\", 2)\n",
        "lexstring(\"'abc\\\\z  \\n\\n\\n'\", 4)\n",
        "lexstring(\"[[\\nalo\\nalo\\n\\n]]\", 5)\n",
        "return true",
    );
    assert!(ok(src));
}

#[test]
fn getinfo_levels_and_errors() {
    assert!(ok(
        "return debug.getinfo(-1) == nil and debug.getinfo(1000) == nil"
    ));
    assert!(ok(
        "return not pcall(debug.getinfo, 1, 'X') and not pcall(debug.getinfo, 0, '>')"
    ));
    // Level 1 names the enclosing local function (its call site names `F`).
    // Avoid a tail call so the caller frame survives to be inspected.
    let src = "\
local function F () return debug.getinfo(1, 'n').name end
local r = F()
return r";
    assert_eq!(eval(src), "F");
    // Level 2 names the function that called the level-1 frame.
    let src = "\
local function inner () return debug.getinfo(2, 'n').name end
local function outer () local r = inner(); return r end
local r = outer()
return r";
    assert_eq!(eval(src), "outer");
}

#[test]
fn getinfo_thread_argument() {
    let src = "\
local co = coroutine.create(function ()
  coroutine.yield()
end)
assert(coroutine.resume(co))
local i = debug.getinfo(co, 0)
assert(i ~= nil and i.currentline == 2, tostring(i and i.currentline))
assert(debug.getinfo(co, 1) == nil)
return true";
    assert!(ok(src));
}

#[test]
fn getinfo_istailcall() {
    let src = "\
local function leaf () return debug.getinfo(1, 't').istailcall end
local function direct () local r = leaf(); return r end
local function tailed () return leaf() end
local a = direct()
local b = tailed()
return a == false and b == true";
    assert!(ok(src));
}

// ---- traceback ----

#[test]
fn traceback_includes_message_and_frames() {
    let src = "\
local function f () return debug.traceback('msg here') end
local s = f()
assert(type(s) == 'string')
assert(string.find(s, '^msg here\\n'))
assert(string.find(s, 'stack traceback:'))
return true";
    assert!(ok(src));
}

#[test]
fn traceback_as_message_handler() {
    let src = "\
local ok, msg = xpcall(function () error('boom') end, debug.traceback)
assert(not ok)
assert(string.find(msg, 'boom'))
assert(string.find(msg, 'stack traceback:'))
return true";
    assert!(ok(src));
}

#[test]
fn traceback_non_string_message_passthrough() {
    assert_eq!(eval("return type(debug.traceback({}))"), "table");
}

// ---- upvalues ----

#[test]
fn upvalue_get_set_and_names() {
    let src = "\
local x = 1
local function f () return x end
assert(debug.getupvalue(f, 1) == 'x')
assert(debug.getupvalue(f, 2) == nil)
assert(debug.setupvalue(f, 1, 42) == 'x')
assert(f() == 42)
assert(debug.setupvalue(f, 2, 1) == nil)
assert(debug.setupvalue(rawget, 1, 1) == nil)
assert(debug.getupvalue(rawget, 1) == nil)
return true";
    assert!(ok(src));
}

#[test]
fn upvalueid_identity() {
    let src = "\
local x = 1
local function foo1 () return x end
local function foo2 () return x end
assert(debug.upvalueid(foo1, 1) ~= nil)
assert(debug.upvalueid(foo1, 2) == nil)
assert(debug.upvalueid(foo1, 1) == debug.upvalueid(foo2, 1))
local y, z = 1, 2
local function g1 () return y end
local function g2 () return z end
assert(debug.upvalueid(g1, 1) ~= debug.upvalueid(g2, 1))
return true";
    assert!(ok(src));
}

#[test]
fn upvaluejoin_shares_cell() {
    let src = "\
local a, b = 10, 20
local fa = function () return a end
local fb = function () return b end
assert(debug.upvalueid(fa, 1) ~= debug.upvalueid(fb, 1))
debug.upvaluejoin(fa, 1, fb, 1)
assert(debug.upvalueid(fa, 1) == debug.upvalueid(fb, 1))
assert(fa() == 20)
b = 30
assert(fa() == 30)
assert(not pcall(debug.upvaluejoin, fa, 3, fb, 1))
assert(not pcall(debug.upvaluejoin, fa, 0, fb, 1))
assert(not pcall(debug.upvaluejoin, rawget, 1, fb, 1))
assert(not pcall(debug.upvaluejoin, fa, 1, {}, 1))
return true";
    assert!(ok(src));
}

// ---- metatables ----

#[test]
fn debug_metatable_bypasses_guard() {
    let src = "\
local real = {__metatable = 'locked'}
local t = setmetatable({}, real)
assert(getmetatable(t) == 'locked')
assert(debug.getmetatable(t) == real)
local other = {}
debug.setmetatable(t, other)
assert(debug.getmetatable(t) == other)
assert(getmetatable(t) == other)
debug.setmetatable(t, nil)
assert(debug.getmetatable(t) == nil)
return true";
    assert!(ok(src));
}

#[test]
fn debug_metatable_on_non_table_types() {
    let src = "\
debug.setmetatable(0, {__index = function () return 42 end})
local n = 7
assert(n.foo == 42)
debug.setmetatable(0, nil)
assert(debug.getmetatable(7) == nil)
return true";
    assert!(ok(src));
}

#[test]
fn getregistry_is_stable_table() {
    assert!(ok(
        "return type(debug.getregistry()) == 'table' and debug.getregistry() == debug.getregistry()"
    ));
}

#[test]
fn gethook_is_nil() {
    assert!(ok("return debug.gethook() == nil"));
}
