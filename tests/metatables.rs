//! M2: metatables/metamethods, pcall/error values, goto.

use slew::{Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua.load(src).unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..1000 {
        match exec.step(lua, 100_000) {
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
    assert_eq!(vals.len(), 1, "expected 1 return value from: {src}");
    lua.display_value(vals[0])
}

fn eval_multi(src: &str) -> Vec<String> {
    let mut lua = Lua::new();
    let vals = run(&mut lua, src);
    vals.iter().map(|v| lua.display_value(*v)).collect()
}

fn run_err(src: &str) -> String {
    let mut lua = Lua::new();
    let chunk = lua.load(src).unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 100_000) {
            Ok(Step::Done(_)) => panic!("expected error: {src}"),
            Ok(Step::Pending) => continue,
            Err(e) => return e.to_string(),
        }
    }
}

// ---- metatable plumbing ----

#[test]
fn set_get_metatable() {
    assert_eq!(
        eval("local t, mt = {}, {} assert(setmetatable(t, mt) == t) return getmetatable(t) == mt"),
        "true"
    );
    assert_eq!(eval("return getmetatable({})"), "nil");
    assert_eq!(eval("local t = setmetatable({}, {__metatable = 'locked'}) return getmetatable(t)"), "locked");
    assert!(run_err("local t = setmetatable({}, {__metatable = 1}) setmetatable(t, {})")
        .contains("protected metatable"));
    assert!(run_err("setmetatable({}, 5)").contains("nil or table expected"));
}

// ---- __index / __newindex ----

#[test]
fn index_metamethods() {
    // __index as table (inheritance chain)
    assert_eq!(
        eval(
            "local base = {greet = 'hello'} \
             local mid = setmetatable({x = 1}, {__index = base}) \
             local obj = setmetatable({}, {__index = mid}) \
             return obj.greet .. obj.x"
        ),
        "hello1"
    );
    // __index as function
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__index = function(t, k) return k .. '!' end}) \
             return t.boo"
        ),
        "boo!"
    );
    // raw hit shadows __index
    assert_eq!(
        eval("local t = setmetatable({a = 1}, {__index = function() return 99 end}) return t.a"),
        "1"
    );
    // __newindex as function: raw table untouched
    assert_eq!(
        eval(
            "local log = {} \
             local t = setmetatable({}, {__newindex = function(t, k, v) log[k] = v * 2 end}) \
             t.x = 21 \
             return (rawget(t, 'x') == nil) and log.x or -1"
        ),
        "42"
    );
    // __newindex as table: write lands there
    assert_eq!(
        eval(
            "local target = {} \
             local t = setmetatable({}, {__newindex = target}) \
             t.x = 5 \
             return target.x"
        ),
        "5"
    );
    // assignment to existing key bypasses __newindex
    assert_eq!(
        eval(
            "local t = setmetatable({x = 1}, {__newindex = function() error('nope') end}) \
             t.x = 2 return t.x"
        ),
        "2"
    );
}

#[test]
fn oop_pattern() {
    assert_eq!(
        eval(
            "local Point = {} \
             Point.__index = Point \
             function Point.new(x, y) return setmetatable({x = x, y = y}, Point) end \
             function Point:dist2() return self.x * self.x + self.y * self.y end \
             local p = Point.new(3, 4) \
             return p:dist2()"
        ),
        "25"
    );
}

// ---- operator metamethods ----

#[test]
fn arith_metamethods() {
    let src = "local mt = {__add = function(a, b) return 'added' end, \
                           __mul = function(a, b) return 42 end, \
                           __unm = function(a) return 'negated' end, \
                           __idiv = function() return 7 end} \
               local t = setmetatable({}, mt) ";
    assert_eq!(eval(&format!("{src} return t + 1")), "added");
    assert_eq!(eval(&format!("{src} return 1 + t")), "added"); // rhs metatable
    assert_eq!(eval(&format!("{src} return t * t")), "42");
    assert_eq!(eval(&format!("{src} return -t")), "negated");
    assert_eq!(eval(&format!("{src} return t // 3")), "7");
}

#[test]
fn compare_metamethods() {
    let src = "local mt = {__eq = function(a, b) return a.id == b.id end, \
                           __lt = function(a, b) return a.id < b.id end, \
                           __le = function(a, b) return a.id <= b.id end} \
               local a = setmetatable({id = 1}, mt) \
               local b = setmetatable({id = 1}, mt) \
               local c = setmetatable({id = 2}, mt) ";
    assert_eq!(eval(&format!("{src} return a == b")), "true");
    assert_eq!(eval(&format!("{src} return a ~= b")), "false");
    assert_eq!(eval(&format!("{src} return a == c")), "false");
    assert_eq!(eval(&format!("{src} return a < c")), "true");
    assert_eq!(eval(&format!("{src} return c <= a")), "false");
    assert_eq!(eval(&format!("{src} return c > a")), "true"); // swaps to __lt
    // __eq result is coerced to boolean
    assert_eq!(
        eval("local t = setmetatable({}, {__eq = function() return 'truthy string' end}) \
              return t == setmetatable({}, getmetatable(t))"),
        "true"
    );
    // __eq not called when raw-equal
    assert_eq!(
        eval("local t = setmetatable({}, {__eq = function() return false end}) return t == t"),
        "true"
    );
}

#[test]
fn concat_len_call_metamethods() {
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__concat = function(a, b) \
                 local l = type(a) == 'table' and 'T' or tostring(a) \
                 local r = type(b) == 'table' and 'T' or tostring(b) \
                 return l .. '|' .. r end}) \
             return 'x' .. t .. 'y'"
        ),
        // right-assoc: t..'y' calls the mm -> "T|y" (a string), then
        // 'x'.."T|y" is plain string concat
        "xT|y"
    );
    assert_eq!(
        eval("local t = setmetatable({1, 2, 3}, {__len = function() return 100 end}) return #t"),
        "100"
    );
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__call = function(self, a, b) return a + b end}) \
             return t(20, 22)"
        ),
        "42"
    );
    // __call passing through multiple returns
    assert_eq!(
        eval_multi(
            "local t = setmetatable({}, {__call = function(self) return 1, 2 end}) return t()"
        ),
        ["1", "2"]
    );
}

#[test]
fn tostring_metamethod() {
    assert_eq!(
        eval("local t = setmetatable({}, {__tostring = function() return 'custom!' end}) \
              return tostring(t)"),
        "custom!"
    );
}

// ---- pcall / error values ----

#[test]
fn pcall_basics() {
    assert_eq!(eval_multi("return pcall(function() return 1, 2 end)"), ["true", "1", "2"]);
    assert_eq!(
        eval_multi("return pcall(function() error('boom') end)"),
        ["false", "chunk:1: boom"]
    );
    assert_eq!(eval_multi("return pcall(function(a, b) return a + b end, 1, 2)"), ["true", "3"]);
    // runtime errors are caught
    assert_eq!(
        eval("local ok, err = pcall(function() return nil + 1 end) return ok"),
        "false"
    );
    // calling a non-function is caught, not fatal
    assert_eq!(eval("return (pcall(5))"), "false");
    // execution continues normally after a caught error
    assert_eq!(
        eval("local n = 0 for i = 1, 3 do local ok = pcall(error, 'x') n = n + 1 end return n"),
        "3"
    );
}

#[test]
fn error_values_roundtrip() {
    // error() with a table value: pcall receives the very table
    assert_eq!(
        eval(
            "local sentinel = {code = 42} \
             local ok, err = pcall(function() error(sentinel) end) \
             return (not ok) and err.code or -1"
        ),
        "42"
    );
    // error with level 0: no position prefix
    assert_eq!(
        eval_multi("return pcall(function() error('raw', 0) end)"),
        ["false", "raw"]
    );
    // assert message value passes through unprefixed
    assert_eq!(
        eval_multi("return pcall(function() assert(false, 'msg') end)"),
        ["false", "msg"]
    );
    assert_eq!(
        eval("local ok, e = pcall(function() assert(nil) end) return e"),
        "chunk:1: assertion failed!"
    );
    // assert passes values through on success
    assert_eq!(eval_multi("return assert(1, 'unused', 3)"), ["1", "unused", "3"]);
}

#[test]
fn pcall_nesting_and_rethrow() {
    assert_eq!(
        eval(
            "local ok1, err1 = pcall(function() \
                 local ok2, err2 = pcall(error, 'inner') \
                 error('outer: ' .. tostring(ok2) .. '/' .. err2, 0) \
             end) \
             return err1"
        ),
        "outer: false/inner"
    );
    // errors in deeply nested calls unwind to the right pcall
    assert_eq!(
        eval(
            "local function lvl3() error('deep', 0) end \
             local function lvl2() lvl3() end \
             local function lvl1() lvl2() end \
             local ok, e = pcall(lvl1) \
             return tostring(ok) .. ':' .. e"
        ),
        "false:deep"
    );
}

#[test]
fn xpcall_handler() {
    assert_eq!(
        eval_multi(
            "return xpcall(function() error('oops', 0) end, function(e) return 'handled: ' .. e end)"
        ),
        ["false", "handled: oops"]
    );
    assert_eq!(
        eval_multi("return xpcall(function(a) return a * 2 end, print, 21)"),
        ["true", "42"]
    );
}

#[test]
fn pcall_catches_metamethod_errors() {
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__index = function() error('mm broke', 0) end}) \
             local ok, e = pcall(function() return t.x end) \
             return tostring(ok) .. ':' .. e"
        ),
        "false:mm broke"
    );
}

#[test]
fn uncaught_error_value_reaches_host() {
    let mut lua = Lua::new();
    let chunk = lua.load("error({code = 7})").unwrap();
    let mut exec = lua.execute(&chunk);
    let err = loop {
        match exec.step(&mut lua, 10_000) {
            Ok(Step::Pending) => continue,
            Ok(Step::Done(_)) => panic!("expected error"),
            Err(slew::Error::Runtime(e)) => break e,
            Err(e) => panic!("unexpected: {e}"),
        }
    };
    // the raised table value is preserved on the public error
    let key = lua.new_string(b"code");
    assert_eq!(lua.table_get(err.value, key), Value::Int(7));
}

// ---- goto ----

#[test]
fn goto_basics() {
    // forward jump skipping code
    assert_eq!(
        eval("local x = 1 goto done x = 2 ::done:: return x"),
        "1"
    );
    // backward jump: loop via goto
    assert_eq!(
        eval(
            "local i = 0 \
             ::top:: \
             i = i + 1 \
             if i < 5 then goto top end \
             return i"
        ),
        "5"
    );
    // continue idiom: label at end of loop body, jumping over locals
    assert_eq!(
        eval(
            "local sum = 0 \
             for i = 1, 10 do \
                 if i % 2 == 0 then goto continue end \
                 local odd = i \
                 sum = sum + odd \
                 ::continue:: \
             end \
             return sum"
        ),
        "25"
    );
    // goto out of nested blocks
    assert_eq!(
        eval(
            "local r = 'a' \
             do do goto out end r = 'b' end \
             r = r .. 'c' \
             ::out:: \
             return r"
        ),
        "a"
    );
}

#[test]
fn goto_scope_errors() {
    let mut lua = Lua::new();
    // jumping into the scope of a local
    assert!(lua
        .load("goto skip local x = 1 ::skip:: return x")
        .unwrap_err()
        .to_string()
        .contains("jumps into the scope"));
    // unmatched label
    assert!(lua
        .load("goto nowhere")
        .unwrap_err()
        .to_string()
        .contains("no visible label"));
    // label in another function is not visible
    assert!(lua
        .load("local function f() ::inner:: end goto inner")
        .unwrap_err()
        .to_string()
        .contains("no visible label"));
    // duplicate label in same block
    assert!(lua
        .load("::l:: ::l::")
        .unwrap_err()
        .to_string()
        .contains("already defined"));
}

#[test]
fn goto_with_upvalues_closes() {
    // jumping back must close per-iteration upvalues so closures stay distinct
    assert_eq!(
        eval(
            "local fs = {} \
             local i = 0 \
             ::top:: \
             i = i + 1 \
             do \
                 local snapshot = i \
                 fs[i] = function() return snapshot end \
             end \
             if i < 3 then goto top end \
             return fs[1]() * 100 + fs[2]() * 10 + fs[3]()"
        ),
        "123"
    );
}

// ---- suspension still exact with metamethods in flight ----

#[test]
fn suspension_through_metamethod_calls() {
    // single-instruction stepping through __index + __add metamethod calls
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local t = setmetatable({}, {__add = function(a, b) return 5 end, \
                                          __index = function(_, k) return 7 end}) \
             local acc = 0 \
             for i = 1, 50 do acc = acc + (t + 1) + t.x end \
             return acc",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 1).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(600)]); // 50 * (5 + 7)
                break;
            }
            Step::Pending => {}
        }
    }
}

#[test]
fn suspension_inside_pcall() {
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local ok, v = pcall(function() \
                 local s = 0 \
                 for i = 1, 1000 do s = s + i end \
                 return s \
             end) \
             return ok and v",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let mut pendings = 0;
    loop {
        match exec.step(&mut lua, 100).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(500500)]);
                break;
            }
            Step::Pending => pendings += 1,
        }
    }
    assert!(pendings > 10, "should suspend many times inside pcall");
}
