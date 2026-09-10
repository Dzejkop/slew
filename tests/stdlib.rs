//! M4: string (patterns, format), table, math, and the Lua prelude
//! (gmatch/gsub/sort).

use slew::{Lua, Step, Value};

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua.load(src).unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
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

fn eval_multi(src: &str) -> Vec<String> {
    let mut lua = Lua::new();
    let vals = run(&mut lua, src);
    vals.iter().map(|v| lua.display_value(*v)).collect()
}

// ---- string ----

#[test]
fn string_basics() {
    assert_eq!(eval("return string.len('hello')"), "5");
    assert_eq!(eval("return string.sub('hello world', 1, 5)"), "hello");
    assert_eq!(eval("return string.sub('hello', -3)"), "llo");
    assert_eq!(eval("return string.sub('hello', 2, -2)"), "ell");
    assert_eq!(eval("return string.upper('mixed Case 42')"), "MIXED CASE 42");
    assert_eq!(eval("return string.lower('MIXED Case')"), "mixed case");
    assert_eq!(eval("return string.rep('ab', 3)"), "ababab");
    assert_eq!(eval("return string.rep('x', 3, '-')"), "x-x-x");
    assert_eq!(eval("return string.reverse('abc')"), "cba");
    assert_eq!(eval("return string.byte('A')"), "65");
    assert_eq!(eval_multi("return string.byte('ABC', 1, 3)"), ["65", "66", "67"]);
    assert_eq!(eval("return string.char(104, 105)"), "hi");
}

#[test]
fn string_method_syntax() {
    // strings share a metatable with __index = string
    assert_eq!(eval("return ('hello'):upper()"), "HELLO");
    assert_eq!(eval("local s = 'a,b,c' return s:sub(1, 1) .. s:len()"), "a5");
    assert_eq!(eval("return ('%d!'):format(42)"), "42!");
}

#[test]
fn string_find_and_match() {
    assert_eq!(eval_multi("return string.find('hello world', 'world')"), ["7", "11"]);
    assert_eq!(eval("return (string.find('abc', 'x'))"), "nil");
    assert_eq!(
        eval_multi("return string.find('key=val', '(%w+)=(%w+)')"),
        ["1", "7", "key", "val"]
    );
    // plain find ignores pattern chars
    assert_eq!(eval_multi("return string.find('a.b', '.', 1, true)"), ["2", "2"]);
    assert_eq!(eval("return string.match('hello 42 world', '%d+')"), "42");
    assert_eq!(
        eval_multi("return string.match('2026-06-10', '(%d+)-(%d+)-(%d+)')"),
        ["2026", "06", "10"]
    );
    assert_eq!(eval("return tostring(string.match('abc', '%d'))"), "nil");
    // init offset
    assert_eq!(eval("return string.match('aXbXc', 'X.', 3)"), "Xc");
}

#[test]
fn string_gmatch() {
    assert_eq!(
        eval(
            "local words = {} \
             for w in string.gmatch('the quick brown fox', '%a+') do words[#words+1] = w end \
             return table.concat(words, '|')"
        ),
        "the|quick|brown|fox"
    );
    assert_eq!(
        eval(
            "local sum = 0 \
             for n in ('10,20,30'):gmatch('%d+') do sum = sum + tonumber(n) end \
             return sum"
        ),
        "60"
    );
    // captures in gmatch
    assert_eq!(
        eval(
            "local t = {} \
             for k, v in ('a=1,b=2'):gmatch('(%w+)=(%w+)') do t[k] = v end \
             return t.a .. t.b"
        ),
        "12"
    );
}

#[test]
fn string_gsub() {
    assert_eq!(
        eval_multi("return string.gsub('hello world', 'o', '0')"),
        ["hell0 w0rld", "2"]
    );
    assert_eq!(eval_multi("return ('aaa'):gsub('a', 'b', 2)"), ["bba", "2"]);
    // %1 capture reference in replacement
    assert_eq!(
        eval("return (string.gsub('hello world', '(%w+)', '<%1>'))"),
        "<hello> <world>"
    );
    // %0 whole match
    assert_eq!(eval("return (string.gsub('abc', '%a', '%0%0'))"), "aabbcc");
    // function replacement (runs through the VM)
    assert_eq!(
        eval("return (string.gsub('1 2 3', '%d', function(d) return tonumber(d) * 2 end))"),
        "2 4 6"
    );
    // table replacement
    assert_eq!(
        eval("return (string.gsub('$name is $age', '%$(%w+)', {name = 'lua', age = 30}))"),
        "lua is 30"
    );
    // nil replacement keeps the match
    assert_eq!(
        eval("return (string.gsub('keep these', '%a+', function() return nil end))"),
        "keep these"
    );
    // anchored pattern replaces only at the start
    assert_eq!(eval_multi("return ('aaa'):gsub('^a', 'b')"), ["baa", "1"]);
    // empty matches advance correctly
    assert_eq!(eval_multi("return ('abc'):gsub('x*', '-')"), ["-a-b-c-", "4"]);
}

#[test]
fn string_format() {
    assert_eq!(eval("return string.format('%d/%d', 7, -3)"), "7/-3");
    assert_eq!(eval("return string.format('%5d|', 42)"), "   42|");
    assert_eq!(eval("return string.format('%-5d|', 42)"), "42   |");
    assert_eq!(eval("return string.format('%05d', 42)"), "00042");
    assert_eq!(eval("return string.format('%+d %+d', 5, -5)"), "+5 -5");
    assert_eq!(eval("return string.format('%x %X %o', 255, 255, 8)"), "ff FF 10");
    assert_eq!(eval("return string.format('%#x', 255)"), "0xff");
    assert_eq!(eval("return string.format('%c%c', 104, 105)"), "hi");
    assert_eq!(eval("return string.format('%.2f', 3.14159)"), "3.14");
    assert_eq!(eval("return string.format('%f', 1)"), "1.000000");
    assert_eq!(eval("return string.format('%e', 1500.0)"), "1.500000e+03");
    assert_eq!(eval("return string.format('%g', 0.00001)"), "1e-05");
    assert_eq!(eval("return string.format('%g', 100000.0)"), "100000");
    assert_eq!(eval("return string.format('%s=%s', 'a', 1)"), "a=1");
    assert_eq!(eval("return string.format('%.3s', 'hello')"), "hel");
    assert_eq!(eval("return string.format('%10s|', 'hi')"), "        hi|");
    assert_eq!(eval("return string.format('%q', 'he said \"hi\"\\n')"),
               "\"he said \\\"hi\\\"\\n\"");
    assert_eq!(eval("return string.format('%%')"), "%");
}

// ---- table ----

#[test]
fn table_insert_remove() {
    assert_eq!(
        eval("local t = {1, 2} table.insert(t, 3) return table.concat(t, ',')"),
        "1,2,3"
    );
    assert_eq!(
        eval("local t = {1, 3} table.insert(t, 2, 2) return table.concat(t, ',')"),
        "1,2,3"
    );
    assert_eq!(
        eval("local t = {1, 2, 3} local v = table.remove(t) return v .. ':' .. table.concat(t, ',')"),
        "3:1,2"
    );
    assert_eq!(
        eval("local t = {1, 2, 3} local v = table.remove(t, 1) return v .. ':' .. table.concat(t, ',')"),
        "1:2,3"
    );
    assert_eq!(eval("return tostring(table.remove({}))"), "nil");
}

#[test]
fn table_concat_pack_unpack() {
    assert_eq!(eval("return table.concat({1, 'a', 2.5}, '-')"), "1-a-2.5");
    assert_eq!(eval("return table.concat({}, ',')"), "");
    assert_eq!(eval("return table.concat({1, 2, 3, 4}, ',', 2, 3)"), "2,3");
    assert_eq!(
        eval("local t = table.pack(10, 20, 30) return t.n .. ':' .. t[1] .. t[2] .. t[3]"),
        "3:102030"
    );
    assert_eq!(eval_multi("return table.unpack({1, 2, 3})"), ["1", "2", "3"]);
    assert_eq!(eval_multi("return table.unpack({1, 2, 3, 4}, 2, 3)"), ["2", "3"]);
    assert_eq!(
        eval("local function f(...) return select('#', ...) end return f(table.unpack({1,2,3}))"),
        "3"
    );
}

#[test]
fn table_sort() {
    assert_eq!(
        eval("local t = {5, 2, 8, 1, 9, 3} table.sort(t) return table.concat(t, ',')"),
        "1,2,3,5,8,9"
    );
    assert_eq!(
        eval(
            "local t = {5, 2, 8, 1} table.sort(t, function(a, b) return a > b end) \
             return table.concat(t, ',')"
        ),
        "8,5,2,1"
    );
    assert_eq!(
        eval("local t = {'banana', 'apple', 'cherry'} table.sort(t) return table.concat(t, ',')"),
        "apple,banana,cherry"
    );
    // large-ish sort (quicksort path, comparator through the VM)
    assert_eq!(
        eval(
            "local t = {} \
             for i = 1, 200 do t[i] = (i * 7919) % 1000 end \
             table.sort(t) \
             for i = 2, 200 do assert(t[i-1] <= t[i]) end \
             return 'sorted'"
        ),
        "sorted"
    );
}

// ---- math ----

#[test]
fn math_basics() {
    assert_eq!(eval("return math.floor(3.7)"), "3");
    assert_eq!(eval("return math.floor(-3.2)"), "-4");
    assert_eq!(eval("return math.ceil(3.2)"), "4");
    assert_eq!(eval("return math.abs(-5)"), "5");
    assert_eq!(eval("return math.abs(-5.5)"), "5.5");
    assert_eq!(eval("return math.sqrt(16)"), "4.0");
    assert_eq!(eval("return math.max(3, 1, 4, 1, 5)"), "5");
    assert_eq!(eval("return math.min(3, 1, 4)"), "1");
    assert_eq!(eval("return math.fmod(7, 3)"), "1");
    assert_eq!(eval_multi("return math.modf(3.7)"), ["3", "0.7"]);
    assert_eq!(eval("return math.tointeger(42.0)"), "42");
    assert_eq!(eval("return tostring(math.tointeger(42.5))"), "nil");
    assert_eq!(eval("return math.type(1) .. '/' .. math.type(1.0)"), "integer/float");
    assert_eq!(eval("return tostring(math.type('x'))"), "nil");
    assert_eq!(eval("return math.huge > 1e308"), "true");
    assert_eq!(eval("return math.maxinteger"), i64::MAX.to_string());
    assert_eq!(eval("return math.ult(-1, 0)"), "false"); // -1 as unsigned is huge
    assert_eq!(eval("return math.log(8, 2)"), "3.0");
    assert!(eval("return math.sin(0)") == "0.0");
}

#[test]
fn math_random_deterministic() {
    // fixed default seed: two fresh states agree
    let a = eval_multi("return math.random(100), math.random(100), math.random()");
    let b = eval_multi("return math.random(100), math.random(100), math.random()");
    assert_eq!(a, b);
    // ranges respected
    assert_eq!(
        eval(
            "for i = 1, 1000 do local r = math.random(10) assert(r >= 1 and r <= 10) end \
             for i = 1, 1000 do local r = math.random(-5, 5) assert(r >= -5 and r <= 5) end \
             for i = 1, 1000 do local r = math.random() assert(r >= 0 and r < 1) end \
             return 'ok'"
        ),
        "ok"
    );
    // reseeding changes the stream deterministically
    let c = eval_multi("math.randomseed(7) return math.random(1000), math.random(1000)");
    let d = eval_multi("math.randomseed(7) return math.random(1000), math.random(1000)");
    assert_eq!(c, d);
    assert_ne!(a[0], c[0]); // overwhelmingly likely
}

// ---- integration: stdlib under strict fuel ----

#[test]
fn gsub_with_function_suspends() {
    // gsub's function-replacement path runs through the VM and must be
    // suspendable mid-replacement
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local s = string.rep('x', 200) \
             local n = 0 \
             local r = s:gsub('x', function() n = n + 1 return tostring(n % 10) end) \
             return #r",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    let mut pendings = 0;
    loop {
        match exec.step(&mut lua, 500).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals, vec![Value::Int(200)]);
                break;
            }
            Step::Pending => {
                pendings += 1;
                assert!(pendings < 100_000, "runaway");
            }
        }
    }
    assert!(pendings > 5, "expected several suspensions, got {pendings}");
}

#[test]
fn sort_comparator_suspends() {
    let mut lua = Lua::new();
    let chunk = lua
        .load(
            "local t = {} \
             for i = 1, 100 do t[i] = (i * 31) % 100 end \
             table.sort(t, function(a, b) return a < b end) \
             return t[1], t[100]",
        )
        .unwrap();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 100).unwrap() {
            Step::Done(vals) => {
                assert_eq!(vals[0], Value::Int(0));
                break;
            }
            Step::Pending => {}
        }
    }
}
