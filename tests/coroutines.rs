//! M3: coroutines and to-be-closed variables.

use slew::{Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..10_000 {
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
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 100_000) {
            Ok(Step::Done(_)) => panic!("expected error: {src}"),
            Ok(Step::Pending) => continue,
            Err(e) => return e.to_string(),
        }
    }
}

// ---- coroutines ----

#[test]
fn create_resume_yield() {
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function(a, b) \
                 local c = coroutine.yield(a + b) \
                 return c * 2 \
             end) \
             local ok1, sum = coroutine.resume(co, 3, 4) \
             local ok2, doubled = coroutine.resume(co, 10) \
             return ok1, sum, ok2, doubled, coroutine.status(co)"
        ),
        ["true", "7", "true", "20", "dead"]
    );
}

#[test]
fn yield_multiple_values_both_directions() {
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function(...) \
                 local x, y = coroutine.yield(...) \
                 return x, y \
             end) \
             local _, a, b, c = coroutine.resume(co, 1, 2, 3) \
             local _, x, y = coroutine.resume(co, 'p', 'q') \
             return a, b, c, x, y"
        ),
        ["1", "2", "3", "p", "q"]
    );
}

#[test]
fn producer_consumer() {
    assert_eq!(
        eval(
            "local producer = coroutine.create(function() \
                 for i = 1, 5 do coroutine.yield(i * i) end \
             end) \
             local total = 0 \
             while true do \
                 local ok, v = coroutine.resume(producer) \
                 if not v then break end \
                 total = total + v \
             end \
             return total"
        ),
        "55" // 1 + 4 + 9 + 16 + 25
    );
}

#[test]
fn statuses() {
    assert_eq!(
        eval_multi(
            "local co \
             local inner_status \
             co = coroutine.create(function() \
                 inner_status = coroutine.status(co) \
                 coroutine.yield() \
             end) \
             local before = coroutine.status(co) \
             coroutine.resume(co) \
             local mid = coroutine.status(co) \
             coroutine.resume(co) \
             local after = coroutine.status(co) \
             return before, inner_status, mid, after"
        ),
        ["suspended", "running", "suspended", "dead"]
    );
    // 'normal' status: a coroutine that resumed another
    assert_eq!(
        eval(
            "local outer \
             local seen \
             local inner = coroutine.create(function() seen = coroutine.status(outer) end) \
             outer = coroutine.create(function() coroutine.resume(inner) end) \
             coroutine.resume(outer) \
             return seen"
        ),
        "normal"
    );
}

#[test]
fn resume_errors_are_reported_not_raised() {
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function() error('inside', 0) end) \
             return coroutine.resume(co)"
        ),
        ["false", "inside"]
    );
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function() end) \
             coroutine.resume(co) \
             return coroutine.resume(co)"
        ),
        ["false", "cannot resume dead coroutine"]
    );
    // resuming yourself
    assert_eq!(
        eval_multi(
            "local co \
             co = coroutine.create(function() return coroutine.resume(co) end) \
             local _, ok, msg = coroutine.resume(co) \
             return ok, msg"
        ),
        ["false", "cannot resume non-suspended coroutine"]
    );
}

#[test]
fn wrap_returns_bare_and_propagates_errors() {
    assert_eq!(
        eval(
            "local gen = coroutine.wrap(function() \
                 for i = 1, 3 do coroutine.yield(i) end \
             end) \
             return gen() + gen() + gen()"
        ),
        "6"
    );
    // errors propagate to the resumer (catchable there with pcall)
    assert_eq!(
        eval_multi(
            "local w = coroutine.wrap(function() error('boom', 0) end) \
             return pcall(function() return w() end)"
        ),
        ["false", "boom"]
    );
}

#[test]
fn yield_from_main_errors() {
    let err = run_err("coroutine.yield(1)");
    assert!(err.contains("outside a coroutine"), "got: {err}");
    assert_eq!(eval("return coroutine.isyieldable()"), "false");
    assert_eq!(
        eval(
            "local co = coroutine.create(function() return coroutine.isyieldable() end) \
              local _, v = coroutine.resume(co) return v"
        ),
        "true"
    );
}

#[test]
fn nested_coroutines() {
    assert_eq!(
        eval(
            "local inner = coroutine.create(function() \
                 coroutine.yield('from inner') \
             end) \
             local outer = coroutine.create(function() \
                 local _, v = coroutine.resume(inner) \
                 coroutine.yield(v .. '/outer') \
             end) \
             local _, got = coroutine.resume(outer) \
             return got"
        ),
        "from inner/outer"
    );
}

#[test]
fn pcall_inside_coroutine() {
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function() \
                 local ok, err = pcall(error, 'caught') \
                 coroutine.yield(ok, err) \
                 return 'finished' \
             end) \
             local _, ok, err = coroutine.resume(co) \
             local _, fin = coroutine.resume(co) \
             return ok, err, fin"
        ),
        ["false", "caught", "finished"]
    );
}

#[test]
fn yield_across_pcall_inside_coroutine() {
    // yielding from within a pcall inside a coroutine (works because pcall
    // is a frame flag, not a host-stack boundary)
    assert_eq!(
        eval_multi(
            "local co = coroutine.create(function() \
                 local ok, v = pcall(function() \
                     local x = coroutine.yield('yielded through pcall') \
                     return x .. '!' \
                 end) \
                 return ok, v \
             end) \
             local _, msg = coroutine.resume(co) \
             local _, ok, v = coroutine.resume(co, 'resumed') \
             return msg, ok, v"
        ),
        ["yielded through pcall", "true", "resumed!"]
    );
}

#[test]
fn coroutine_suspends_on_fuel_and_resumes() {
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local co = coroutine.create(function() \
                 local n = 0 \
                 for i = 1, 100000 do n = n + 1 end \
                 coroutine.yield(n) \
             end) \
             local _, v = coroutine.resume(co) \
             return v",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let mut pendings = 0;
    loop {
        match exec.step(&mut lua, 1_000).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(100000)]);
                break;
            }
            Step::Pending => {
                pendings += 1;
                assert!(pendings < 10_000, "runaway");
            }
        }
    }
    // the long loop runs INSIDE the coroutine: most suspensions happen there
    assert!(pendings > 50, "suspended {pendings} times");
}

#[test]
fn generic_for_over_wrapped_iterator() {
    assert_eq!(
        eval(
            "local function range(n) \
                 return coroutine.wrap(function() \
                     for i = 1, n do coroutine.yield(i) end \
                 end) \
             end \
             local sum = 0 \
             for i in range(10) do sum = sum + i end \
             return sum"
        ),
        "55"
    );
}

// ---- to-be-closed variables ----

#[test]
fn close_on_scope_exit() {
    assert_eq!(
        eval(
            "local log = {} \
             do \
                 local a <close> = setmetatable({}, {__close = function() log[#log+1] = 'a' end}) \
                 local b <close> = setmetatable({}, {__close = function() log[#log+1] = 'b' end}) \
                 log[#log+1] = 'body' \
             end \
             return log[1] .. log[2] .. log[3]"
        ),
        "bodyba" // reverse declaration order
    );
}

#[test]
fn close_on_return_and_break() {
    assert_eq!(
        eval(
            "local closed = false \
             local function f() \
                 local r <close> = setmetatable({}, {__close = function() closed = true end}) \
                 return 'result' \
             end \
             local v = f() \
             return v .. '/' .. tostring(closed)"
        ),
        "result/true"
    );
    assert_eq!(
        eval(
            "local n = 0 \
             for i = 1, 3 do \
                 local r <close> = setmetatable({}, {__close = function() n = n + 1 end}) \
                 if i == 2 then break end \
             end \
             return n"
        ),
        "2" // closed at end of iteration 1 and at the break in iteration 2
    );
}

#[test]
fn close_receives_error_object_during_unwind() {
    assert_eq!(
        eval_multi(
            "local seen \
             local ok, err = pcall(function() \
                 local r <close> = setmetatable({}, {__close = function(self, e) seen = e end}) \
                 error('unwound', 0) \
             end) \
             return ok, err, seen"
        ),
        ["false", "unwound", "unwound"]
    );
}

#[test]
fn close_false_and_nil_allowed() {
    assert_eq!(
        eval("do local x <close> = nil local y <close> = false end return 'ok'"),
        "ok"
    );
    let err = run_err("local x <close> = 42");
    assert!(err.contains("non-closable"), "got: {err}");
    let mut lua = Lua::new();
    assert!(
        lua.load("local a <close>, b <close> = nil, nil")
            .unwrap_err()
            .to_string()
            .contains("multiple")
    );
}

#[test]
fn error_inside_close_propagates() {
    assert_eq!(
        eval_multi(
            "return pcall(function() \
                 local r <close> = setmetatable({}, {__close = function() error('close failed', 0) end}) \
                 return 'unreachable result' \
             end)"
        ),
        ["false", "close failed"]
    );
}

#[test]
fn close_suspends_correctly() {
    // __close handlers run as frames: stepping one instruction at a time
    // through scope exits with closes must work
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local n = 0 \
             for i = 1, 10 do \
                 local r <close> = setmetatable({}, {__close = function() n = n + i end}) \
             end \
             return n",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 1).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(55)]);
                break;
            }
            Step::Pending => {}
        }
    }
}

#[test]
fn generic_for_closes_fourth_explist_value() {
    // locals.lua: the generic-for explist may yield iterator, state, control
    // and a fourth to-be-closed value; it must be closed on every exit path.
    // Normal completion.
    assert_eq!(
        eval(
            "local closed = 0 \
             for k in next, {1, 2, 3}, nil, \
                 setmetatable({}, {__close = function() closed = closed + 1 end}) do end \
             return closed"
        ),
        "1"
    );
    // `break`.
    assert_eq!(
        eval(
            "local closed = 0 \
             for k in next, {1, 2, 3}, nil, \
                 setmetatable({}, {__close = function() closed = closed + 1 end}) do break end \
             return closed"
        ),
        "1"
    );
    // Error unwinding, with the original error preserved.
    assert_eq!(
        eval_multi(
            "local closed = 0 \
             local ok, err = pcall(function() \
                 for k in next, {1, 2, 3}, nil, \
                     setmetatable({}, {__close = function() closed = closed + 1 end}) do \
                     error('boom', 0) \
                 end \
             end) \
             return ok, err, closed"
        ),
        ["false", "boom", "1"]
    );
    // A closing value returned as the 4th result of a custom iterator factory
    // (locals.lua's `open`), exercised across normal and broken loops.
    assert_eq!(
        eval_multi(
            "local open = 0 \
             local function iter(n) \
                 local i = n \
                 return function() i = i - 1; if i > 0 then return i end end, \
                        nil, nil, \
                        setmetatable({}, {__close = function() open = open + 1 end}) \
             end \
             local s = 0 \
             for i in iter(10) do s = s + i end \
             local b = 0 \
             for i in iter(10) do if i < 5 then break end b = b + i end \
             return s, b, open"
        ),
        ["45", "35", "2"]
    );
}
