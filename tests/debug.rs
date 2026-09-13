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
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
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

#[test]
fn getinfo_transfer_fields() {
    // PUC 5.4's default `what` is "flnSrtu", so 'r' fields are present and
    // zero for plain calls (transfers only describe hook/call movement).
    let src = "\
local i = debug.getinfo(1)
assert(i.ftransfer == 0, tostring(i.ftransfer))
assert(i.ntransfer == 0, tostring(i.ntransfer))
local function f () return debug.getinfo(1, 'r') end
local r = f()
assert(r.ftransfer == 0 and r.ntransfer == 0)
-- Requesting only 'S' omits them.
assert(debug.getinfo(1, 'S').ftransfer == nil)
assert(debug.getinfo(f, 'r').ftransfer == 0)
return true";
    assert!(ok(src));
}

#[test]
fn getinfo_lastlinedefined_and_activelines() {
    // `lastlinedefined` is the line of the closing `end`, and the implicit
    // final RETURN puts that line into `activelines` (PUC).
    let src = "\
local function f()
 local a = 1
 return a
end
local function g()
end
local i = debug.getinfo(f, 'S')
assert(i.linedefined == 1, i.linedefined)
assert(i.lastlinedefined == 4, i.lastlinedefined)
local lf = debug.getinfo(f, 'L').activelines
assert(lf[2] and lf[3] and lf[4], 'f activelines')
assert(lf[1] == nil)
local gi = debug.getinfo(g, 'S')
assert(gi.linedefined == 5, gi.linedefined)
assert(gi.lastlinedefined == 6, gi.lastlinedefined)
local lg = debug.getinfo(g, 'L').activelines
assert(lg[6] and lg[5] == nil, 'g activelines')
local h = function() return 1 end
local hi = debug.getinfo(h, 'S')
assert(hi.linedefined == 18 and hi.lastlinedefined == 18)
-- The main chunk keeps PUC's `lastlinedefined == 0`.
assert(debug.getinfo(1, 'S').lastlinedefined == 0)
return true";
    assert!(ok(src));
}

#[test]
fn getinfo_level_coercion() {
    assert!(ok("return not pcall(debug.getinfo, 0.5)"));
    assert!(ok("return not pcall(debug.getinfo, 0/0)"));
    // Integral floats are still valid levels.
    assert!(ok("return debug.getinfo(1.0) ~= nil"));
    assert!(ok("local ok, e = pcall(debug.getinfo, 0.5)\n\
         return not ok and string.find(e, 'integer representation') ~= nil"));
    // The optional thread shifts the argument number but not the semantics.
    let src = "\
local co = coroutine.create(function () coroutine.yield() end)
coroutine.resume(co)
local ok, err = pcall(debug.getinfo, co, 0.5)
assert(not ok and string.find(err, 'integer representation'), err)
return true";
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
fn traceback_names_close_metamethod() {
    // A directly-invoked `__close` handler that raises is reported by the
    // xpcall message handler as `metamethod 'close'`: PUC runs the handler at
    // the error site, so the close frame is still on the stack.
    let src = "\
local function func2close (f) return setmetatable({}, {__close = f}) end
local function foo ()
  local x <close> = func2close(function () error('@x') end)
end
local ok, msg = xpcall(foo, debug.traceback)
assert(not ok)
assert(string.find(msg, 'in metamethod .close.'), msg)
return true";
    assert!(ok(src));
}

#[test]
fn tbc_error_messages_match_puc() {
    // PUC names the offending local and flags a missing `__close` as a
    // metamethod error.
    assert_eq!(
        eval(
            "local ok, e = pcall(function () local x <close> = {} end)\n\
             return tostring(not ok and string.find(e, \"variable 'x' got a non%-closable value\") ~= nil)"
        ),
        "true"
    );
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__close = print})\n\
             local ok, e = pcall(function () local x <close> = t; getmetatable(t).__close = nil end)\n\
             return tostring(not ok and string.find(e, \"metamethod 'close'\") ~= nil)"
        ),
        "true"
    );
    assert_eq!(
        eval(
            "local ok, e = pcall(function () local t = setmetatable({}, {__close = 4}); local x <close> = t end)\n\
             return tostring(not ok and string.find(e, \"metamethod 'close'\") ~= nil)"
        ),
        "true"
    );
}

#[test]
fn traceback_non_string_message_passthrough() {
    assert_eq!(eval("return type(debug.traceback({}))"), "table");
    // A non-string message is returned untouched, so the level argument is
    // never coerced (PUC).
    assert!(ok("return debug.traceback({}, 0.5) ~= nil"));
}

#[test]
fn traceback_level_semantics() {
    // A level past the stack yields only the header.
    let src = "\
local s = debug.traceback('m', 100)
local _, nl = string.gsub(s, '\\n', '\\n')
assert(nl == 1, 'expected only the header, got ' .. nl)
return true";
    assert!(ok(src));
    // The default level still shows frames.
    let src = "\
local function f () return debug.traceback('m') end
local s = f()
local _, nl = string.gsub(s, '\\n', '\\n')
assert(nl > 1, 'expected frames, got ' .. nl)
return true";
    assert!(ok(src));
    // Non-integral levels are rejected.
    assert!(ok("local ok, e = pcall(debug.traceback, 'm', 0.5)\n\
         return not ok and string.find(e, 'integer representation') ~= nil"));
}

#[test]
fn traceback_suspended_coroutine() {
    // Must not panic, and a level past the coroutine's stack shows no frames.
    let src = "\
local co = coroutine.create(function () coroutine.yield() end)
coroutine.resume(co)
local s = debug.traceback(co)
assert(type(s) == 'string' and string.find(s, 'stack traceback:'))
local s2 = debug.traceback(co, 'm', 100)
local _, nl = string.gsub(s2, '\\n', '\\n')
assert(nl == 1, 'expected header only, got ' .. nl)
return true";
    assert!(ok(src));
}

// ---- upvalues ----

#[test]
fn upvalue_get_set_and_names() {
    let src = "\
local x = 1
local function f () return x end
assert(debug.getupvalue(f, 1) == 'x')
-- Out-of-range indices return *zero* values (PUC); `select('#', ...)` sees 0.
assert(select('#', debug.getupvalue(f, 2)) == 0)
assert(select('#', debug.getupvalue(f, 0)) == 0)
assert(debug.setupvalue(f, 1, 42) == 'x')
assert(f() == 42)
assert(select('#', debug.setupvalue(f, 2, 1)) == 0)
assert(select('#', debug.setupvalue(f, 0, 1)) == 0)
-- Native (C) functions are valid functions, not type errors; they simply
-- have no upvalues, so getupvalue/setupvalue report zero values and
-- upvalueid pushes nil (PUC `checkupval`/`auxupvalue`).
assert(debug.getupvalue(rawget, 1) == nil)
assert(select('#', debug.getupvalue(rawget, 1)) == 0)
assert(select('#', debug.getupvalue(rawget, 99)) == 0)
assert(debug.setupvalue(rawget, 1, 1) == nil)
assert(select('#', debug.setupvalue(rawget, 1, 1)) == 0)
assert(debug.upvalueid(rawget, 1) == nil)
return true";
    assert!(ok(src));
}

#[test]
fn upvalue_api_argument_validation() {
    // PUC validates the index (arg #2) before the function (arg #1), and
    // setupvalue checks the value (arg #3) first.
    let src = "\
local x = 1
local function f () return x end
assert(not pcall(debug.getupvalue, f))
assert(not pcall(debug.getupvalue, f, 'x'))
assert(not pcall(debug.getupvalue, f, 1.5))
assert(not pcall(debug.getupvalue, f, 0/0))
assert(not pcall(debug.getupvalue, {}, 1))
assert(not pcall(debug.setupvalue, f))
assert(not pcall(debug.setupvalue, f, 1))
assert(not pcall(debug.setupvalue, f, 'x', 1))
assert(not pcall(debug.setupvalue, f, 1.5, 1))
assert(not pcall(debug.setupvalue, {}, 1, 2))
assert(not pcall(debug.upvalueid, f))
assert(not pcall(debug.upvalueid, f, 'x'))
assert(not pcall(debug.upvalueid, f, 1.5))
assert(not pcall(debug.upvalueid, {}, 1))
-- upvaluejoin validates both index/function pairs.
assert(not pcall(debug.upvaluejoin, f, 1, f, 0))
assert(not pcall(debug.upvaluejoin, {}, 1, f, 1))
-- non-integral numbers report the integer-representation failure.
local ok, err = pcall(debug.getupvalue, f, 1.5)
assert(not ok and string.find(err, 'integer representation'), err)
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

// ---- closing across protection boundaries ----

#[test]
fn coroutine_close_runs_tbc_after_suspended_pcall() {
    // A coroutine suspended inside a pcall still runs the to-be-closed
    // handlers of frames below the protection boundary when it is closed.
    let src = "\
local function func2close (f) return setmetatable({}, {__close = f}) end
local closed = false
local co = coroutine.create(function ()
  return pcall(function ()
    local y <close> = func2close(function () closed = true end)
    coroutine.yield()
  end)
end)
assert(coroutine.resume(co))
assert(coroutine.close(co))
assert(closed == true)
return true";
    assert!(ok(src));
}

#[test]
fn return_hook_fires_for_c_and_lua_returns_in_order() {
    // A return hook set inside a `__close` handler observes the C calls and
    // the Lua function returning, in order.
    let src = "\
local function func2close (f) return setmetatable({}, {__close = f}) end
local trace = {}
local function hook (event)
  trace[#trace + 1] = event
end
local function foo (...)
  local x <close> = func2close(function () trace[#trace + 1] = 'x' end)
  local y <close> = func2close(function () debug.sethook(hook, 'r') end)
  return ...
end
local t = {foo(10, 20, 30)}
debug.sethook()
assert(t[1] == 10 and t[2] == 20 and t[3] == 30)
assert(table.concat(trace, ',') == 'return,return,x,return,return', table.concat(trace, ','))
return true";
    assert!(ok(src));
}

#[test]
fn sethook_call_line_return_and_gethook() {
    // Call/line/return hooks on a coroutine, and `gethook`'s three results.
    let src = "\
local co = coroutine.create(function ()
  coroutine.yield(10)
  return 20
end)
local trace = {}
local function dotrace (event) trace[#trace + 1] = event end
debug.sethook(co, dotrace, 'clr')
repeat until not coroutine.resume(co)
assert(table.concat(trace, ',') == 'call,line,call,return,line,return',
       table.concat(trace, ','))
local h, m, c = debug.gethook(co)
assert(h == dotrace and m == 'crl' and c == 0)
assert(debug.gethook() == nil)
return true";
    assert!(ok(src));
}
