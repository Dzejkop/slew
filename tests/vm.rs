use suslua::{Lua, Step, Value};

/// Runs a script to completion with a generous fuel budget; panics if it
/// doesn't finish. Returns the script's return values.
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

/// Runs `src` and returns the single return value rendered via tostring rules.
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

// ---- arithmetic & 5.4 number semantics ----

#[test]
fn numbers_54_semantics() {
    assert_eq!(eval("return 1 + 2"), "3");
    assert_eq!(eval("return 7 / 2"), "3.5"); // / is always float
    assert_eq!(eval("return 7 // 2"), "3");
    assert_eq!(eval("return -7 // 2"), "-4"); // floor division
    assert_eq!(eval("return 7 % 3"), "1");
    assert_eq!(eval("return -7 % 3"), "2"); // Lua mod follows divisor sign
    assert_eq!(eval("return 7.0 // 2"), "3.0"); // float floor div stays float
    assert_eq!(eval("return 2 ^ 10"), "1024.0"); // ^ is always float
    assert_eq!(eval("return 1 + 0.5"), "1.5");
    assert_eq!(eval("return 9223372036854775807 + 1"), "-9223372036854775808"); // wraps
    assert_eq!(eval("return 1 == 1.0"), "true");
    assert_eq!(eval("return 1 < 1.5"), "true");
    assert_eq!(eval("return 0.0 == -0.0"), "true");
    assert_eq!(eval("return 10 // 0.0"), "inf");
    assert_eq!(eval("return 3 & 5"), "1");
    assert_eq!(eval("return 3 | 5"), "7");
    assert_eq!(eval("return 3 ~ 5"), "6");
    assert_eq!(eval("return ~0"), "-1");
    assert_eq!(eval("return 1 << 4"), "16");
    assert_eq!(eval("return -1 >> 56"), "255"); // logical shift
    assert_eq!(eval("return 1 << 64"), "0");
    assert_eq!(eval("return 2.0 & 3"), "2"); // exact float converts
    assert_eq!(eval("return 0/0 ~= 0/0"), "true"); // NaN
    assert_eq!(eval("return (0/0) < 1"), "false");
    assert_eq!(eval("return 1 < (0/0)"), "false");
}

#[test]
fn integer_division_by_zero_errors() {
    assert!(run_err("return 1 // 0").contains("n//0"));
    assert!(run_err("return 1 % 0").contains("n%0"));
    assert!(run_err("return 1.5 & 2").contains("no integer representation"));
    assert!(run_err("return {} + 1").contains("arithmetic"));
}

#[test]
fn strings_and_concat() {
    assert_eq!(eval("return 'a' .. 'b' .. 'c'"), "abc");
    assert_eq!(eval("return 'x=' .. 1 .. ',' .. 1.5"), "x=1,1.5");
    assert_eq!(eval("return #'hello'"), "5");
    assert_eq!(eval("return 'abc' < 'abd'"), "true");
    assert_eq!(eval("return 'Z' < 'a'"), "true"); // byte order
    assert_eq!(eval("return 'a' == 'a'"), "true");
    assert!(run_err("return {} .. 'x'").contains("concatenate"));
}

#[test]
fn tostring_conversions() {
    assert_eq!(eval("return tostring(nil)"), "nil");
    assert_eq!(eval("return tostring(true)"), "true");
    assert_eq!(eval("return tostring(42)"), "42");
    assert_eq!(eval("return tostring(1.0)"), "1.0");
    assert_eq!(eval("return tostring(1/3)"), "0.33333333333333");
    assert_eq!(eval("return tonumber('42')"), "42");
    assert_eq!(eval("return tonumber('  -0x10  ')"), "-16");
    assert_eq!(eval("return tonumber('3.5e2')"), "350.0");
    assert_eq!(eval("return tonumber('zz', 36)"), "1295");
    assert_eq!(eval("return tonumber('hello')"), "nil");
    assert_eq!(eval("return type(3) .. type('') .. type(nil)"), "numberstringnil");
}

// ---- control flow ----

#[test]
fn control_flow() {
    assert_eq!(
        eval("local s = 0 for i = 1, 10 do s = s + i end return s"),
        "55"
    );
    assert_eq!(
        eval("local s = 0 for i = 10, 1, -2 do s = s + i end return s"),
        "30"
    );
    assert_eq!(eval("local s = 0 for i = 1, 0 do s = s + 1 end return s"), "0");
    assert_eq!(
        eval("local s = 0.0 for i = 1.0, 2.0, 0.5 do s = s + i end return s"),
        "4.5"
    );
    assert_eq!(
        eval("local n, i = 0, 1 while i <= 100 do n = n + i i = i + 1 end return n"),
        "5050"
    );
    assert_eq!(
        eval("local i = 0 repeat i = i + 1 until i >= 5 return i"),
        "5"
    );
    assert_eq!(
        eval("local done repeat local x = 5 done = x until done return done"),
        "5" // until sees body locals
    );
    assert_eq!(
        eval(
            "local r = '' for i = 1, 10 do if i % 2 == 0 then r = r .. i elseif i == 5 then break end end return r"
        ),
        "24"
    );
    assert_eq!(
        eval("if false then return 1 elseif nil then return 2 else return 3 end"),
        "3"
    );
    assert_eq!(eval("return false or nil"), "nil");
    assert_eq!(eval("return nil and 1 or 2"), "2");
    assert_eq!(eval("return 0 and 'zero is truthy'"), "zero is truthy");
}

#[test]
fn numeric_for_edge_cases() {
    // loop var is local to the loop and reset each run; loop to i64::MAX must terminate
    assert_eq!(
        eval("local n = 0 for i = 9223372036854775805, 9223372036854775807 do n = n + 1 end return n"),
        "3"
    );
    // float limit on an integer loop
    assert_eq!(eval("local n = 0 for i = 1, 3.5 do n = n + 1 end return n"), "3");
    assert!(run_err("for i = 1, 10, 0 do end").contains("step is zero"));
    assert!(run_err("for i = 1, 'x' do end").contains("must be a number"));
}

// ---- functions, closures, multrets ----

#[test]
fn functions_and_recursion() {
    assert_eq!(
        eval("local function fib(n) if n < 2 then return n end return fib(n-1) + fib(n-2) end return fib(20)"),
        "6765"
    );
    assert_eq!(
        eval("local function f(a, b) return b, a end local x, y = f(1, 2) return x * 10 + y"),
        "21"
    );
    // multiple returns adjust
    assert_eq!(eval_multi("local function f() return 1, 2, 3 end return f()"), ["1", "2", "3"]);
    assert_eq!(eval("local function f() return 1, 2, 3 end return (f())"), "1"); // parens truncate
    assert_eq!(
        eval_multi("local function f() return 1, 2 end return f(), 10"),
        ["1", "10"] // non-tail call truncates to one value
    );
    assert_eq!(
        eval_multi("local function f() return 1, 2 end return 10, f()"),
        ["10", "1", "2"] // tail position expands
    );
    // missing args become nil, extras dropped
    assert_eq!(eval("local function f(a, b) return tostring(b) end return f(1)"), "nil");
    assert_eq!(eval("local function f(a) return a end return f(1, 2, 3)"), "1");
}

#[test]
fn closures_and_upvalues() {
    assert_eq!(
        eval(
            "local function counter() local n = 0 return function() n = n + 1 return n end end \
             local c = counter() c() c() return c()"
        ),
        "3"
    );
    // two closures sharing one upvalue
    assert_eq!(
        eval(
            "local function make() local n = 0 return function() n = n + 1 end, function() return n end end \
             local inc, get = make() inc() inc() return get()"
        ),
        "2"
    );
    // each loop iteration captures a fresh variable
    assert_eq!(
        eval(
            "local fs = {} for i = 1, 3 do fs[i] = function() return i end end \
             return fs[1]() * 100 + fs[2]() * 10 + fs[3]()"
        ),
        "123"
    );
    // upvalue through two nesting levels
    assert_eq!(
        eval(
            "local x = 1 local function outer() local function inner() x = x + 1 return x end return inner end \
             return outer()()"
        ),
        "2"
    );
}

#[test]
fn varargs() {
    assert_eq!(
        eval("local function f(...) return select('#', ...) end return f(1, nil, 3)"),
        "3"
    );
    assert_eq!(
        eval("local function f(...) local a, b = ... return a + b end return f(10, 20, 30)"),
        "30"
    );
    assert_eq!(
        eval("local function f(...) return ... end return (select(2, f(1, 2, 3)))"),
        "2"
    );
    assert_eq!(
        eval("local function f(a, ...) return a + select('#', ...) end return f(10, 1, 1, 1)"),
        "13"
    );
    assert_eq!(eval("local function f(...) local t = {...} return #t end return f(1, 2, 3)"), "3");
}

// ---- tables ----

#[test]
fn tables() {
    assert_eq!(eval("local t = {1, 2, 3} return #t"), "3");
    assert_eq!(eval("local t = {a = 1, b = 2} return t.a + t.b"), "3");
    assert_eq!(eval("local t = {[2] = 'two', 'one'} return t[1] .. t[2]"), "onetwo");
    assert_eq!(eval("local t = {} t[1.0] = 'x' return t[1]"), "x"); // key normalization
    assert_eq!(eval("local t = {} t.x = 10 t.x = t.x + 1 return t.x"), "11");
    assert_eq!(
        eval("local t = {10, 20, 30} local s = 0 for i, v in ipairs(t) do s = s + i * v end return s"),
        "140"
    );
    assert_eq!(
        eval(
            "local t = {a = 1, b = 2, c = 3} local s = 0 for k, v in pairs(t) do s = s + v end return s"
        ),
        "6"
    );
    assert_eq!(
        eval("local t = {1, 2, f = 3} local n = 0 for k in pairs(t) do n = n + 1 end return n"),
        "3"
    );
    // multret expansion in constructor
    assert_eq!(
        eval("local function f() return 2, 3, 4 end local t = {1, f()} return #t"),
        "4"
    );
    // nested
    assert_eq!(eval("local t = {x = {y = {z = 42}}} return t.x.y.z"), "42");
    // method calls
    assert_eq!(
        eval("local obj = {n = 10} function obj:get() return self.n end return obj:get()"),
        "10"
    );
    assert!(run_err("local t = {} t[nil] = 1").contains("table index is nil"));
    assert!(run_err("local x = nil return x.field").contains("attempt to index a nil value"));
    assert!(run_err("local x = 5 x()").contains("attempt to call a number value"));
}

#[test]
fn big_table_constructor_flushes() {
    // more than one SetList batch (50 per flush)
    let src = format!(
        "local t = {{{}}} return #t + t[60]",
        (1..=120).map(|i| i.to_string()).collect::<Vec<_>>().join(", ")
    );
    assert_eq!(eval(&src), "180");
}

// ---- globals ----

#[test]
fn globals() {
    assert_eq!(eval("x = 42 return x"), "42");
    assert_eq!(eval("return tostring(undefined_global)"), "nil");
    assert_eq!(
        eval("function double(n) return 2 * n end return double(21)"),
        "42"
    );
    let mut lua = Lua::new();
    run(&mut lua, "answer = 6 * 7");
    assert_eq!(lua.get_global("answer"), Value::Int(42));
    lua.set_global("from_rust", Value::Int(7));
    let vals = run(&mut lua, "return from_rust * 6");
    assert_eq!(vals, vec![Value::Int(42)]);
}

#[test]
fn const_attrib_enforced() {
    let mut lua = Lua::new();
    let err = lua.load("local x <const> = 1 x = 2").unwrap_err();
    assert!(err.to_string().contains("const"));
}

// ---- suspension: the point of this library ----

#[test]
fn infinite_loop_suspends() {
    let mut lua = Lua::new();
    let chunk = lua.load("n = 0 while true do n = n + 1 end").unwrap();
    let mut exec = lua.execute(&chunk);
    for _ in 0..10 {
        assert_eq!(exec.step(&mut lua, 1_000).unwrap(), Step::Pending);
    }
    // the script made real progress while staying interruptible
    let Value::Int(n) = lua.get_global("n") else { panic!() };
    assert!(n > 100, "loop should have progressed, n = {n}");
    assert!(!exec.is_finished());
}

#[test]
fn fuel_is_deterministic() {
    // identical fuel schedules suspend at exactly the same point
    let observe = |budgets: &[u64]| -> i64 {
        let mut lua = Lua::new();
        let chunk = lua.load("n = 0 for i = 1, 1000000 do n = n + 1 end").unwrap();
        let mut exec = lua.execute(&chunk);
        for &b in budgets {
            let _ = exec.step(&mut lua, b).unwrap();
        }
        match lua.get_global("n") {
            Value::Int(n) => n,
            _ => panic!(),
        }
    };
    let a = observe(&[1000, 1000, 1000]);
    let b = observe(&[1000, 1000, 1000]);
    assert_eq!(a, b, "same fuel must reach the same state");
    // and fuel maps monotonically to progress
    let c = observe(&[1000, 1000, 1000, 1000]);
    assert!(c > a);
}

#[test]
fn fuel_proportional_progress() {
    let progress = |fuel: u64| -> i64 {
        let mut lua = Lua::new();
        let chunk = lua.load("n = 0 while true do n = n + 1 end").unwrap();
        let mut exec = lua.execute(&chunk);
        assert_eq!(exec.step(&mut lua, fuel).unwrap(), Step::Pending);
        match lua.get_global("n") {
            Value::Int(n) => n,
            _ => panic!(),
        }
    };
    let p1 = progress(10_000);
    let p2 = progress(20_000);
    // double fuel ≈ double progress (same per-iteration cost)
    let ratio = p2 as f64 / p1 as f64;
    assert!((1.9..=2.1).contains(&ratio), "ratio {ratio}, p1 {p1}, p2 {p2}");
}

#[test]
fn suspension_mid_call_resumes_correctly() {
    // suspend inside nested function calls; state must survive arbitrarily
    // small budgets (here: 1 fuel unit per step — one instruction at a time)
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local function add(a, b) return a + b end \
             local s = 0 \
             for i = 1, 100 do s = add(s, i) end \
             return s",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let mut steps = 0u64;
    loop {
        match exec.step(&mut lua, 1).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(5050)]);
                break;
            }
            Step::Pending => {
                steps += 1;
                assert!(steps < 1_000_000, "runaway");
            }
        }
    }
    assert!(steps > 1000, "should have taken many single-instruction steps");
}

#[test]
fn zero_fuel_makes_no_progress() {
    let mut lua = Lua::new();
    let chunk = lua.load("n = 0 while true do n = n + 1 end").unwrap();
    let mut exec = lua.execute(&chunk);
    assert_eq!(exec.step(&mut lua, 0).unwrap(), Step::Pending);
    assert_eq!(lua.get_global("n"), Value::Nil); // not even `n = 0` ran
}

#[test]
fn step_after_done_errors() {
    let mut lua = Lua::new();
    let chunk = lua.load("return 1").unwrap();
    let mut exec = lua.execute(&chunk);
    assert_eq!(exec.step(&mut lua, 100).unwrap(), Step::Done(vec![Value::Int(1)]));
    assert!(exec.step(&mut lua, 100).is_err());
}

#[test]
fn two_executions_share_globals_but_not_control() {
    let mut lua = Lua::new();
    let writer = lua.load("i = (i or 0) while true do i = i + 1 end").unwrap();
    let reader = lua.load("return i").unwrap();
    let mut w = lua.execute(&writer);
    let _ = w.step(&mut lua, 5_000).unwrap();
    let mut r = lua.execute(&reader);
    let Step::Done(vals) = r.step(&mut lua, 1_000).unwrap() else {
        panic!("reader should finish")
    };
    let Value::Int(i) = vals[0] else { panic!() };
    assert!(i > 0);
    // writer remains suspended and resumable
    assert_eq!(w.step(&mut lua, 1_000).unwrap(), Step::Pending);
}

#[test]
fn runtime_error_reports_line() {
    let err = run_err("local x = 1\nlocal y = 2\nreturn x + {}");
    assert!(err.contains("chunk:3:"), "got: {err}");
}

#[test]
fn deep_recursion_hits_depth_limit_not_host_stack() {
    let err = run_err("local function f() return f() + 1 end return f()");
    assert!(err.contains("stack overflow"), "got: {err}");
}
