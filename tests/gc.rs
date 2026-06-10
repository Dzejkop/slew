//! M5: mark-sweep GC and the memory budget.

use suslua::{Error, Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua.load(src).unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..100_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(vals)) => return vals,
            Ok(Step::Pending) => continue,
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish: {src}");
}

#[test]
fn collects_garbage_tables_and_strings() {
    let mut lua = Lua::new();
    let baseline = lua.gc();
    run(
        &mut lua,
        "local keep = {}
         for i = 1, 10000 do
             local garbage = {i, i + 1, tostring(i) .. '-junk-' .. i}
             if i % 1000 == 0 then keep[#keep + 1] = garbage end
         end
         kept = keep",
    );
    let before = lua.memory_used();
    let after = lua.gc();
    assert!(
        after < before / 2,
        "GC should reclaim most garbage: before {before}, after {after}"
    );
    // within ~2x of the fresh baseline (kept data + slack survives)
    assert!(after < baseline * 3 + 200_000, "after {after}, baseline {baseline}");
    // survivors intact
    let vals = run(&mut lua, "return #kept, kept[1][3]");
    assert_eq!(vals[0], Value::Int(10));
    assert_eq!(lua.display_value(vals[1]), "1000-junk-1000");
}

#[test]
fn survivors_keep_identity_and_metatables() {
    let mut lua = Lua::new();
    run(
        &mut lua,
        "obj = setmetatable({n = 1}, {__index = function() return 'meta!' end})
         for i = 1, 5000 do local _ = {'garbage' .. i} end",
    );
    lua.gc();
    let vals = run(&mut lua, "return obj.n, obj.missing_field");
    assert_eq!(vals[0], Value::Int(1));
    assert_eq!(lua.display_value(vals[1]), "meta!");
}

#[test]
fn closures_and_upvalues_survive() {
    let mut lua = Lua::new();
    run(
        &mut lua,
        "local n = 0
         counter = function() n = n + 1 return n end
         counter() counter()
         for i = 1, 5000 do local _ = function() return i end end",
    );
    lua.gc();
    let vals = run(&mut lua, "return counter()");
    assert_eq!(vals, vec![Value::Int(3)]); // upvalue state preserved
}

#[test]
fn suspended_coroutines_survive_gc() {
    let mut lua = Lua::new();
    run(
        &mut lua,
        "co = coroutine.create(function()
             local acc = 'state'
             coroutine.yield()
             coroutine.yield(acc .. '-preserved')
         end)
         coroutine.resume(co)
         for i = 1, 5000 do local _ = {i} end",
    );
    lua.gc();
    let vals = run(&mut lua, "local _, v = coroutine.resume(co) return v");
    assert_eq!(lua.display_value(vals[0]), "state-preserved");
}

#[test]
fn unreachable_coroutines_are_collected() {
    let mut lua = Lua::new();
    let baseline = lua.gc();
    run(
        &mut lua,
        "for i = 1, 200 do
             local co = coroutine.create(function() coroutine.yield(string.rep('x', 10000)) end)
             coroutine.resume(co)
         end",
    );
    let after = lua.gc();
    assert!(
        after < baseline + 500_000,
        "dropped coroutines (with big stacks) should be reclaimed: {after} vs baseline {baseline}"
    );
}

#[test]
fn anchors_pin_host_values() {
    let mut lua = Lua::new();
    let v = lua.new_string(b"host-held string that nothing in lua references");
    lua.anchor(v);
    lua.gc();
    assert_eq!(
        lua.str_bytes(v).unwrap(),
        b"host-held string that nothing in lua references"
    );
    lua.unanchor(v);
}

#[test]
fn suspended_execution_state_survives_gc() {
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local acc = {}
             for i = 1, 100000 do acc[#acc + 1] = i % 100 end
             local sum = 0
             for _, v in ipairs(acc) do sum = sum + v end
             return sum",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let mut steps = 0;
    loop {
        match exec.step(&mut lua, 10_000).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(4950 * 1000)]);
                break;
            }
            Step::Pending => {
                steps += 1;
                // collect aggressively *between* steps: the suspended
                // execution is a root, its state must survive
                lua.gc();
                assert!(steps < 1_000_000);
            }
        }
    }
    assert!(steps > 3, "expected multiple suspensions");
}

#[test]
fn aborted_execution_releases_roots() {
    let mut lua = Lua::new();
    let baseline = lua.gc();
    let chunk = lua
        .load("local big = {} for i = 1, 50000 do big[i] = 'data' .. i end while true do end")
        .unwrap();
    let mut exec = lua.execute(&chunk);
    while !matches!(exec.step(&mut lua, 100_000).unwrap(), Step::Pending) {}
    // give it enough fuel to build the table
    for _ in 0..50 {
        let _ = exec.step(&mut lua, 100_000).unwrap();
    }
    let with_live = lua.gc();
    assert!(with_live > baseline + 1_000_000, "table should be live: {with_live}");
    exec.abort(&mut lua);
    let after_abort = lua.gc();
    assert!(
        after_abort < baseline + 300_000,
        "aborting must release the execution's heap: {after_abort} vs {baseline}"
    );
}

#[test]
fn memory_limit_enforced() {
    let mut lua = Lua::new();
    lua.memory_limit = Some(lua.gc() + 2_000_000); // ~2 MB of headroom
    lua.gc_alloc_threshold = 1_000; // check often
    let chunk = lua
        .load(
            "local t = {}
             local i = 0
             while true do
                 i = i + 1
                 t[i] = 'consume memory ' .. i .. string.rep('x', 100)
             end",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let err = loop {
        match exec.step(&mut lua, 1_000_000) {
            Ok(Step::Pending) => continue,
            Ok(Step::Done(_)) => panic!("expected memory error"),
            Err(Error::Runtime(e)) => break e,
            Err(e) => panic!("unexpected: {e}"),
        }
    };
    assert!(err.message.contains("not enough memory"), "got: {}", err.message);
}

#[test]
fn gc_disabled_when_threshold_zero() {
    let mut lua = Lua::new();
    lua.gc_alloc_threshold = 0;
    run(&mut lua, "for i = 1, 10000 do local _ = {tostring(i)} end");
    let before = lua.memory_used();
    let after = lua.gc(); // manual collection still works
    assert!(after < before, "manual gc should reclaim: {after} vs {before}");
}

#[test]
fn determinism_unaffected_by_gc_pressure() {
    // same fuel schedule, with and without aggressive GC: identical results
    let observe = |threshold: usize| -> i64 {
        let mut lua = Lua::new();
        lua.gc_alloc_threshold = threshold;
        let chunk = lua
            .load("n = 0 for i = 1, 30000 do local t = {i} n = n + t[1] % 7 end")
            .unwrap();
        let mut exec = lua.execute(&chunk);
        for _ in 0..10 {
            if let Step::Done(_) = exec.step(&mut lua, 100_000).unwrap() {
                break;
            }
        }
        match lua.get_global("n") {
            Value::Int(n) => n,
            v => panic!("{v:?}"),
        }
    };
    assert_eq!(observe(500), observe(1_000_000));
}
