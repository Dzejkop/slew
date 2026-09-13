//! M5: mark-sweep GC and the memory budget.

use slew::{Error, Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..100_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(vals)) => return vals,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
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
    assert!(
        after < baseline * 3 + 200_000,
        "after {after}, baseline {baseline}"
    );
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
    let mut lua = Lua::<()>::new();
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
            Step::Waiting(_) => panic!("unexpected native wait"),
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
    assert!(
        with_live > baseline + 1_000_000,
        "table should be live: {with_live}"
    );
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
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
            Ok(Step::Done(_)) => panic!("expected memory error"),
            Err(Error::Runtime(e)) => break e,
            Err(e) => panic!("unexpected: {e}"),
        }
    };
    assert!(
        err.message.contains("not enough memory"),
        "got: {}",
        err.message
    );
}

#[test]
fn gc_disabled_when_threshold_zero() {
    let mut lua = Lua::new();
    lua.gc_alloc_threshold = 0;
    run(&mut lua, "for i = 1, 10000 do local _ = {tostring(i)} end");
    let before = lua.memory_used();
    let after = lua.gc(); // manual collection still works
    assert!(
        after < before,
        "manual gc should reclaim: {after} vs {before}"
    );
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

// ---- Phase 6: weak tables, finalizers, collectgarbage, coroutine.close ----

#[test]
fn collectgarbage_options() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local a = collectgarbage('isrunning')
         collectgarbage('stop')
         local b = collectgarbage('isrunning')
         local c = collectgarbage('restart')
         local d = collectgarbage('isrunning')
         local e = collectgarbage('setpause', 123)
         local f = collectgarbage('setstepmul', 45)
         local cnt = collectgarbage('count')
         local col = collectgarbage('collect')
         local st = collectgarbage('step')
         local st0 = collectgarbage('step', 0)
         return a, b, c, d, e, f, type(cnt) == 'number' and cnt > 0, col, st, st0",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(vals[1], Value::Bool(false));
    assert_eq!(vals[2], Value::Int(0));
    assert_eq!(vals[3], Value::Bool(true));
    assert_eq!(vals[4], Value::Int(200));
    assert_eq!(vals[5], Value::Int(100));
    assert_eq!(vals[6], Value::Bool(true));
    assert_eq!(vals[7], Value::Int(0));
    // A step always finishes a collection cycle (stop-the-world collector),
    // so `step` reports true; PUC returns true once a cycle completed.
    assert_eq!(vals[8], Value::Bool(true));
    assert_eq!(vals[9], Value::Bool(true));
}

#[test]
fn collectgarbage_step_terminates_cycle_loops() {
    let mut lua = Lua::new();
    // Mirrors gc.lua's `dosteps`: the loop must terminate because a step
    // reports a completed collection cycle.
    let vals = run(
        &mut lua,
        "local function dosteps(siz)
           collectgarbage()
           local i = 0
           repeat i = i + 1 until collectgarbage('step', siz)
           return i
         end
         collectgarbage('stop')
         local small = dosteps(2)
         local big = dosteps(20000)
         return small, big, collectgarbage('step', 20000)",
    );
    assert_eq!(vals[0], Value::Int(1));
    assert_eq!(vals[1], Value::Int(1));
    assert_eq!(vals[2], Value::Bool(true));
}

#[test]
fn collectgarbage_number_option_is_invalid_option() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local ok, err = pcall(collectgarbage, 5)
         local ok2, err2 = pcall(collectgarbage, {})
         return ok, err, ok2, err2",
    );
    assert_eq!(vals[0], Value::Bool(false));
    assert!(lua.display_value(vals[1]).contains("invalid option '5'"));
    assert_eq!(vals[2], Value::Bool(false));
    assert!(
        lua.display_value(vals[3])
            .contains("string expected, got table")
    );
}

#[test]
fn coroutine_close_argument_errors_match_puc() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local ok, err = pcall(coroutine.close)
         local ok2, err2 = pcall(coroutine.close, 5)
         return ok, err, ok2, err2",
    );
    assert_eq!(vals[0], Value::Bool(false));
    assert!(
        lua.display_value(vals[1])
            .contains("bad argument #1 to 'coroutine.close' (thread expected, got no value)"),
        "got: {}",
        lua.display_value(vals[1])
    );
    assert_eq!(vals[2], Value::Bool(false));
    assert!(
        lua.display_value(vals[3])
            .contains("bad argument #1 to 'coroutine.close' (thread expected, got number)"),
        "got: {}",
        lua.display_value(vals[3])
    );
}

#[test]
fn collectgarbage_mode_switch_and_errors() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local a = collectgarbage('incremental')  -- previous is generational
         local b = collectgarbage('generational') -- previous is incremental
         local ok = pcall(collectgarbage, 'bogus')
         return a, b, ok",
    );
    assert_eq!(lua.display_value(vals[0]), "generational");
    assert_eq!(lua.display_value(vals[1]), "incremental");
    assert_eq!(vals[2], Value::Bool(false));
}

#[test]
fn weak_values_are_cleared_and_strings_kept() {
    let mut lua = Lua::new();
    // The collector scans the whole thread stack, so a value can survive in a
    // dead temporary slot (see DESIGN.md "root precision"); creating many in
    // a loop reuses those slots, so all but a few are reclaimed.
    let vals = run(
        &mut lua,
        "local t = setmetatable({}, {__mode = 'v'})
         for i = 1, 20 do
           local v = {}
           t[i] = v
           v = nil
         end
         t.str = 'kept'
         collectgarbage()
         local n = 0
         for _ in pairs(t) do n = n + 1 end
         return n <= 2, t.str",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(lua.display_value(vals[1]), "kept");
}

#[test]
fn weak_keys_and_ephemerons() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local t = setmetatable({}, {__mode = 'k'})
         local k = {}
         t[k] = 'value'
         collectgarbage()
         local alive = t[k] ~= nil   -- key still referenced
         k = nil
         collectgarbage()
         local n = 0
         for _ in pairs(t) do n = n + 1 end
         return alive, n",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(vals[1], Value::Int(0));
}

#[test]
fn weak_kv_with_numeric_key_clears_value() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local x = {[1] = {}}
         setmetatable(x, {__mode = 'kv'})
         collectgarbage()
         return rawget(x, 1) == nil, getmetatable(x).__mode",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(lua.display_value(vals[1]), "kv");
}

#[test]
fn gc_finalizer_runs_and_resurrects() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local saved
         local o = setmetatable({}, {__gc = function(self) saved = self end})
         o = nil
         collectgarbage()
         local resurrected = saved ~= nil
         saved = nil
         collectgarbage()   -- the resurrected object is now collected
         return resurrected",
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn gc_finalizer_is_one_shot() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local n = 0
         local o = setmetatable({}, {__gc = function() n = n + 1 end})
         o = nil
         collectgarbage()
         collectgarbage()
         collectgarbage()
         return n",
    );
    assert_eq!(vals[0], Value::Int(1));
}

#[test]
fn gc_finalizer_order_is_lifo() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local order = {}
         local a = setmetatable({}, {__gc = function() order[#order+1] = 'a' end})
         local b = setmetatable({}, {__gc = function() order[#order+1] = 'b' end})
         a = nil; b = nil
         collectgarbage()
         return order[1], order[2]",
    );
    assert_eq!(lua.display_value(vals[0]), "b");
    assert_eq!(lua.display_value(vals[1]), "a");
}

#[test]
fn coroutine_is_yieldable_and_tostring() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local main = coroutine.running()
         local co = coroutine.create(function() coroutine.yield() end)
         local r = {coroutine.isyieldable(main), coroutine.isyieldable(co),
                    string.find(tostring(co), 'thread') ~= nil}
         local ok = pcall(coroutine.isyieldable, 5)
         return r[1], r[2], r[3], ok",
    );
    assert_eq!(vals[0], Value::Bool(false));
    assert_eq!(vals[1], Value::Bool(true));
    assert_eq!(vals[2], Value::Bool(true));
    assert_eq!(vals[3], Value::Bool(false));
}

#[test]
fn coroutine_close_runs_tbc_and_returns() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local log = {}
         local co = coroutine.create(function()
           local x <close> = setmetatable({}, {__close = function(_, err) log[#log+1] = err == nil and 'nil' or err end})
           coroutine.yield('y')
         end)
         local ok, v = coroutine.resume(co)
         local st = coroutine.close(co)
         local st2 = coroutine.close(co)
         local running = pcall(coroutine.close, coroutine.running())
         return ok, v, st, st2, #log, log[1], coroutine.status(co), running",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(lua.display_value(vals[1]), "y");
    assert_eq!(vals[2], Value::Bool(true));
    assert_eq!(vals[3], Value::Bool(true));
    assert_eq!(vals[4], Value::Int(1));
    assert_eq!(lua.display_value(vals[5]), "nil");
    assert_eq!(lua.display_value(vals[6]), "dead");
    assert_eq!(vals[7], Value::Bool(false));
}

#[test]
fn coroutine_close_reports_close_error() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local co = coroutine.create(function()
           local z <close> = setmetatable({}, {__close = function() error('boom') end})
           coroutine.yield()
         end)
         coroutine.resume(co)
         local st, msg = coroutine.close(co)
         return st, msg",
    );
    assert_eq!(vals[0], Value::Bool(false));
    assert!(lua.display_value(vals[1]).contains("boom"));
}

#[test]
fn next_skips_collected_weak_entries() {
    let mut lua = Lua::new();
    // The result mirrors a test pattern: a weak-keyed table whose only key
    // dies; `next`/`pairs` must not report the collected entry.
    let vals = run(
        &mut lua,
        "local t = setmetatable({}, {__mode = 'k'})
         local k = {}
         t[k] = true
         k = nil
         collectgarbage()
         local first = next(t)
         local n = 0
         for _ in pairs(t) do n = n + 1 end
         return first == nil, n",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(vals[1], Value::Int(0));
}

#[test]
fn next_survives_deleting_current_key_mid_iteration() {
    let mut lua = Lua::new();
    // Mirrors nextvar.lua's "erasing values": walk the whole hash part,
    // clearing the current key and collecting on every step. Every entry must
    // still be visited exactly once (the cursor must not be invalidated).
    let vals = run(
        &mut lua,
        "local t = {}
         for i = 1, 50 do t['k' .. i] = i end
         local seen, n = {}, 0
         for k, v in pairs(t) do
           n = n + 1
           seen[k] = true
           assert(t[k] == v)
           t[k] = nil
           collectgarbage()
           assert(t[k] == nil)
         end
         for i = 1, 50 do assert(seen['k' .. i]) end
         return n, next(t) == nil",
    );
    assert_eq!(vals[0], Value::Int(50));
    assert_eq!(vals[1], Value::Bool(true));
}

#[test]
fn next_reports_current_key_after_prior_deletions() {
    let mut lua = Lua::new();
    // nextvar.lua's GC-of-deleted-keys case: after deleting every prior key,
    // `next(t)` (from nil) must report the key the for-loop is currently on,
    // even after collectgarbage collects those deleted keys.
    let vals = run(
        &mut lua,
        "local t = {}
         t[string.rep('a', 50)] = 'a'
         t[string.rep('b', 50)] = 'b'
         t[string.rep('c', 50)] = 'c'
         local count, ok = 0, true
         for k, v in pairs(t) do
           count = count + 1
           local k1 = next(t)          -- all previous keys were deleted
           ok = ok and (k == k1)
           t[k] = nil
           collectgarbage('collect')
         end
         return count, ok, next(t) == nil",
    );
    assert_eq!(vals[0], Value::Int(3));
    assert_eq!(vals[1], Value::Bool(true));
    assert_eq!(vals[2], Value::Bool(true));
}

#[test]
fn reinserting_visited_key_does_not_loop_forever() {
    let mut lua = Lua::new();
    // Deleting the current key and re-adding it with the same value must not
    // make `pairs` revisit it: each key is seen once and the loop terminates.
    let vals = run(
        &mut lua,
        "local t = {a = 1, b = 2, c = 3}
         local n = 0
         for k, v in pairs(t) do
           n = n + 1
           assert(n <= 10)         -- guard against an infinite traversal
           t[k] = nil
           t[k] = v
           collectgarbage()
         end
         return n",
    );
    assert_eq!(vals[0], Value::Int(3));
}

// ---------------------------------------------------------------------------
// Precise root collection: only the live register window of each frame is a
// root, not the whole thread stack.
// ---------------------------------------------------------------------------

/// Reproduces the upstream `gc.lua` weak-kv probe that used to stall at
/// `assert(i == 4)`: many collectable keys/values are created in the chunk's
/// own registers, then dropped. After `collectgarbage` only the four live
/// entries (three referenced locals plus one string pair) may remain, and
/// clearing `x,y,z` must leave a single entry.
#[test]
fn precise_roots_weak_kv_drops_dead_temporaries() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local lim = 8
         local a = {}; setmetatable(a, {__mode = 'kv'})
         local x, y, z = {}, {}, {}
         a[1], a[2], a[3] = x, y, z
         a[string.rep('$', 11)] = string.rep('$', 11)
         for i = 4, lim do a[i] = {} end
         for i = 1, lim do a[{}] = i end
         for i = 1, lim do local t = {}; a[t] = t end
         collectgarbage()
         local first = 0
         for _ in pairs(a) do first = first + 1 end
         x, y, z = nil
         collectgarbage()
         return first, next(a) == string.rep('$', 11)",
    );
    assert_eq!(vals[0], Value::Int(4), "live entries must not be collected");
    assert_eq!(vals[1], Value::Bool(true), "dead entries must be reclaimed");
}

/// Values held only through a weak table are reclaimed, while a value that is
/// still a live local of the collecting frame survives.
#[test]
fn precise_roots_keep_live_locals_and_drop_dead_temporaries() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local w = setmetatable({}, {__mode = 'v'})
         local live = {}
         w[1] = live
         local function two() return {}, {} end
         w[2], w[3] = two()   -- dead result temporaries linger above the top
         collectgarbage()
         local live_kept = w[1] == live
         live = nil
         collectgarbage()
         local n = 0
         for _ in pairs(w) do n = n + 1 end
         return live_kept, n",
    );
    assert_eq!(vals[0], Value::Bool(true), "live local must survive");
    assert_eq!(vals[1], Value::Int(0), "dead temps and dropped local gone");
}

/// A weakly referenced value is rooted by a local of a *suspended coroutine*
/// frame, and dead temporaries of that frame are not.
#[test]
fn suspended_coroutine_roots_only_live_registers() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local w = setmetatable({}, {__mode = 'v'})
         local co = coroutine.create(function()
           local live = {}
           w[1] = live
           -- two dead result temporaries; only the first is clobbered by the
           -- `yield()` call slot, so the second one would be over-retained by a
           -- whole-stack scan.
           w[2], w[3] = (function() return {}, {} end)()
           coroutine.yield()
           return live ~= nil
         end)
         coroutine.resume(co)
         collectgarbage()
         local live_kept = w[1] ~= nil
         local dead_dropped = w[2] == nil and w[3] == nil
         local ok, kept_after = coroutine.resume(co)
         return live_kept, dead_dropped, kept_after",
    );
    assert_eq!(
        vals[0],
        Value::Bool(true),
        "suspended frame local must survive"
    );
    assert_eq!(vals[1], Value::Bool(true), "dead temps must be reclaimed");
    assert_eq!(
        vals[2],
        Value::Bool(true),
        "coroutine must resume correctly"
    );
}

/// A value passed through a proper tail call must stay rooted while the
/// tail-called frame runs and collects.
#[test]
fn live_values_survive_under_tail_calls() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local w = setmetatable({}, {__mode = 'v'})
         local function g(x)
           collectgarbage()
           return x
         end
         local function f()
           local v = {}
           w[1] = v
           return g(v)       -- proper tail call replaces f's frame
         end
         local r = f()
         return r ~= nil, w[1] ~= nil",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(
        vals[1],
        Value::Bool(true),
        "tail-call value must stay rooted"
    );
}

/// To-be-closed unwinding stages return values outside the register window;
/// a `collectgarbage` from the `__close` handler must not lose them.
#[test]
fn return_values_survive_close_handler_collection() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local saved
         local function f()
           local x <close> = setmetatable({}, {
             __close = function(_, err)
               collectgarbage()
               saved = {err}
             end})
           return 'payload'
         end
         local r = f()
         return r, type(saved)",
    );
    assert_eq!(lua.display_value(vals[0]), "payload");
    assert_eq!(lua.display_value(vals[1]), "table");
}

/// PUC treats `collectgarbage` as invalid while a collection cycle (including
/// `__gc` handlers) is running and returns a single `nil`; the upstream
/// `gc.lua` reentrancy check asserts this.
#[test]
fn collectgarbage_in_finalizer_returns_nil() {
    let mut lua = Lua::new();
    let vals = run(
        &mut lua,
        "local res = true
         local kind = 'unset'
         setmetatable({}, {__gc = function()
           local r = collectgarbage()
           res = r
           kind = type(r)
         end})
         collectgarbage()
         return res == nil, kind",
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(lua.display_value(vals[1]), "nil");
}
