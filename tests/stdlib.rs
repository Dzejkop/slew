//! M4: string (patterns, format), table, math, and the Lua prelude
//! (gmatch/gsub/sort).

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
        match exec.step(&mut lua, 1_000_000) {
            Ok(Step::Done(_)) => panic!("expected error: {src}"),
            Ok(Step::Pending) => continue,
            Err(e) => return e.to_string(),
        }
    }
}

// ---- string ----

#[test]
fn string_basics() {
    assert_eq!(eval("return string.len('hello')"), "5");
    assert_eq!(eval("return string.sub('hello world', 1, 5)"), "hello");
    assert_eq!(eval("return string.sub('hello', -3)"), "llo");
    assert_eq!(eval("return string.sub('hello', 2, -2)"), "ell");
    assert_eq!(
        eval("return string.upper('mixed Case 42')"),
        "MIXED CASE 42"
    );
    assert_eq!(eval("return string.lower('MIXED Case')"), "mixed case");
    assert_eq!(eval("return string.rep('ab', 3)"), "ababab");
    assert_eq!(eval("return string.rep('x', 3, '-')"), "x-x-x");
    assert_eq!(eval("return string.reverse('abc')"), "cba");
    assert_eq!(eval("return string.byte('A')"), "65");
    assert_eq!(
        eval_multi("return string.byte('ABC', 1, 3)"),
        ["65", "66", "67"]
    );
    assert_eq!(eval("return string.char(104, 105)"), "hi");
}

#[test]
fn string_method_syntax() {
    // strings share a metatable with __index = string
    assert_eq!(eval("return ('hello'):upper()"), "HELLO");
    assert_eq!(
        eval("local s = 'a,b,c' return s:sub(1, 1) .. s:len()"),
        "a5"
    );
    assert_eq!(eval("return ('%d!'):format(42)"), "42!");
}

#[test]
fn string_find_and_match() {
    assert_eq!(
        eval_multi("return string.find('hello world', 'world')"),
        ["7", "11"]
    );
    assert_eq!(eval("return (string.find('abc', 'x'))"), "nil");
    assert_eq!(
        eval_multi("return string.find('key=val', '(%w+)=(%w+)')"),
        ["1", "7", "key", "val"]
    );
    // plain find ignores pattern chars
    assert_eq!(
        eval_multi("return string.find('a.b', '.', 1, true)"),
        ["2", "2"]
    );
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
    assert_eq!(
        eval_multi("return ('abc'):gsub('x*', '-')"),
        ["-a-b-c-", "4"]
    );
}

#[test]
fn string_format() {
    assert_eq!(eval("return string.format('%d/%d', 7, -3)"), "7/-3");
    assert_eq!(eval("return string.format('%5d|', 42)"), "   42|");
    assert_eq!(eval("return string.format('%-5d|', 42)"), "42   |");
    assert_eq!(eval("return string.format('%05d', 42)"), "00042");
    assert_eq!(eval("return string.format('%+d %+d', 5, -5)"), "+5 -5");
    assert_eq!(
        eval("return string.format('%x %X %o', 255, 255, 8)"),
        "ff FF 10"
    );
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
    assert_eq!(
        eval("return string.format('%q', 'he said \"hi\"\\n')"),
        "\"he said \\\"hi\\\"\\n\""
    );
    assert_eq!(eval("return string.format('%%')"), "%");
}

#[test]
fn string_format_hex_float() {
    assert_eq!(eval("return string.format('%a', 1.0)"), "0x1p+0");
    assert_eq!(eval("return string.format('%a', 0.5)"), "0x1p-1");
    assert_eq!(eval("return string.format('%a', 0.0)"), "0x0p+0");
    assert_eq!(eval("return string.format('%a', -0.0)"), "-0x0p+0");
    assert_eq!(eval("return string.format('%A', 12)"), "0X1.8P+3");
    assert_eq!(eval("return string.format('%+.2A', 12)"), "+0X1.80P+3");
    assert_eq!(eval("return string.format('%.4A', -12)"), "-0X1.8000P+3");
    // full precision round-trips
    assert_eq!(
        eval("return tonumber(string.format('%a', 0.1)) == 0.1"),
        "true"
    );
    assert_eq!(
        eval("return tonumber(string.format('%a', 1e30)) == 1e30"),
        "true"
    );
    assert_eq!(
        eval("return tonumber(string.format('%a', 1/3)) == 1/3"),
        "true"
    );
    assert_eq!(eval("return string.format('%a', 1/0)"), "inf");
    assert_eq!(eval("return string.format('%A', -1/0)"), "-INF");
}

#[test]
fn string_format_pointer_validation() {
    // '%p' only accepts the '-' flag and no precision (PUC checkformat)
    let e = run_err("return string.format('%+p', {})");
    assert!(e.contains("invalid conversion specification"), "{e}");
    let e = run_err("return string.format('%.3p', {})");
    assert!(e.contains("invalid conversion specification"), "{e}");
    let e = run_err("return string.format('%#p', {})");
    assert!(e.contains("invalid conversion specification"), "{e}");
    // widths and '-' flag remain valid
    assert_eq!(eval("return #string.format('%90p', {})"), "90");
    assert_eq!(eval("return #string.format('%-60p', {})"), "60");
    assert_eq!(eval("return string.format('%p', nil)"), "(null)");
}

#[test]
fn string_rep_overflow() {
    assert_eq!(eval("return string.rep('teste', 0)"), "");
    let e = run_err("return string.rep('ab', math.maxinteger)");
    assert!(e.contains("resulting string too large"), "{e}");
    let e = run_err("return string.rep('a', math.maxinteger)");
    assert!(e.contains("resulting string too large"), "{e}");
}

// ---- string.pack / unpack / packsize ----

#[test]
fn string_pack_round_trips() {
    assert_eq!(eval("return string.packsize('j')"), "8");
    assert_eq!(eval("return string.packsize('n')"), "8");
    assert_eq!(eval("return string.packsize('i4i4')"), "8");
    // no alignment by default (PUC's `initheader` sets maxalign = 1)
    assert_eq!(eval("return string.packsize('i1i8')"), "9");
    assert_eq!(eval("return string.packsize('i3')"), "3");
    assert_eq!(eval("return string.packsize('xxi4')"), "6");
    // `!n` enables alignment (PUC stores it directly in maxalign)
    assert_eq!(eval("return string.packsize('!8i1i8')"), "16");
    assert_eq!(eval("return string.packsize('!4i1i4')"), "8");
    assert_eq!(eval("return string.packsize('!8i1d')"), "16");
    // a later `!n` replaces the previous maximum (PUC semantics)
    assert_eq!(eval("return string.packsize('!8 i1 !2 i8')"), "10");
    assert_eq!(eval("return string.packsize('xx')"), "2");
    assert_eq!(eval("return string.packsize('<i4>i4')"), "8");
    let bytes = "return string.byte(string.pack('>i4', 0x01020304), 1, 4)";
    assert_eq!(eval_multi(bytes), ["1", "2", "3", "4"]);
    let bytes_le = "return string.byte(string.pack('<i4', 0x01020304), 1, 4)";
    assert_eq!(eval_multi(bytes_le), ["4", "3", "2", "1"]);
    // no padding is inserted between unaligned fields by default
    assert_eq!(
        eval_multi("return string.byte(string.pack('<i1i2', 2, 3), 1, 3)"),
        ["2", "3", "0"]
    );
    assert_eq!(
        eval_multi("return string.unpack('i4', string.pack('i4', -42))"),
        ["-42", "5"]
    );
    assert_eq!(
        eval_multi("return string.unpack('>i2', string.pack('>i2', -2))"),
        ["-2", "3"]
    );
    assert_eq!(
        eval_multi("return string.unpack('J', string.pack('J', -1))"),
        ["-1", "9"]
    );
    assert_eq!(
        eval("return string.unpack('z', string.pack('z', 'abc'))"),
        "abc"
    );
    assert_eq!(
        eval_multi("return string.unpack('z', string.pack('z', 'abc'))"),
        ["abc", "5"]
    );
    assert_eq!(
        eval_multi("return string.unpack('s1', string.pack('s1', 'hey'))"),
        ["hey", "5"]
    );
    assert_eq!(
        eval_multi("return string.unpack('i4i4', string.pack('i4i4', 7, 9), 1)"),
        ["7", "9", "9"]
    );
    assert_eq!(
        eval("return math.type(string.unpack('i4', string.pack('i4', 7)))"),
        "integer"
    );
    assert_eq!(
        eval("return math.type(string.unpack('d', string.pack('d', 7)))"),
        "float"
    );
    assert_eq!(
        eval("return string.unpack('d', string.pack('d', 1.5)) == 1.5"),
        "true"
    );
    assert_eq!(
        eval("return string.unpack('f', string.pack('f', 1.5)) == 1.5"),
        "true"
    );
}

#[test]
fn string_pack_errors() {
    assert!(run_err("return string.packsize('z')").contains("variable-length format"));
    assert!(run_err("return string.packsize('s')").contains("variable-length format"));
    assert!(run_err("return string.pack('b', 200)").contains("integer overflow"));
    assert!(run_err("return string.pack('B', -1)").contains("unsigned overflow"));
    assert!(run_err("return string.pack('c2', 'abc')").contains("string longer than given size"));
    assert!(run_err("return string.pack('z', 'a\\0b')").contains("string contains zeros"));
    assert!(
        run_err("return string.pack('s1', string.rep('a', 300))")
            .contains("string length does not fit in given size")
    );
    assert!(run_err("return string.unpack('i4', 'ab')").contains("data string too short"));
    assert!(run_err("return string.unpack('z', 'abc')").contains("unfinished string"));
    assert!(
        run_err("return string.unpack('i4', 'abcd', 9)").contains("initial position out of string")
    );
    assert!(
        run_err("return string.unpack('i4', 'abcd', math.maxinteger)")
            .contains("initial position out of string")
    );
    // PUC's posrelatI clips positions below -len to the start rather than erroring
    assert_eq!(
        eval_multi("return string.unpack('i4', 'abcd', math.mininteger)"),
        ["1684234849", "5"]
    );
    // position len+1 is the one-past-the-end sentinel and unpacks zero fields
    assert_eq!(
        eval_multi("return string.unpack('c0', 'abcd', 5)"),
        ["", "5"]
    );
    assert!(run_err("return string.packsize('i0')").contains("integral size (0) out of limits"));
    assert!(run_err("return string.packsize('Q')").contains("invalid format option 'Q'"));
    assert!(run_err("return string.packsize('c')").contains("missing size for format option 'c'"));
    assert!(run_err("return string.packsize('Xz')").contains("invalid next option for option 'X'"));
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
        eval(
            "local t = {1, 2, 3} local v = table.remove(t) return v .. ':' .. table.concat(t, ',')"
        ),
        "3:1,2"
    );
    assert_eq!(
        eval(
            "local t = {1, 2, 3} local v = table.remove(t, 1) return v .. ':' .. table.concat(t, ',')"
        ),
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
    assert_eq!(
        eval_multi("return table.unpack({1, 2, 3})"),
        ["1", "2", "3"]
    );
    assert_eq!(
        eval_multi("return table.unpack({1, 2, 3, 4}, 2, 3)"),
        ["2", "3"]
    );
    assert_eq!(
        eval("local function f(...) return select('#', ...) end return f(table.unpack({1,2,3}))"),
        "3"
    );
    // a range wider than the result cap must error, not overflow the host
    assert!(
        run_err("return table.unpack({}, math.mininteger, math.maxinteger)")
            .contains("too many results")
    );
    assert!(run_err("return table.unpack({}, 0, (1 << 31) - 1)").contains("too many results"));
    // empty ranges still return nothing
    assert!(eval("return select('#', table.unpack({}, 10, 6))") == "0");
}

#[test]
fn ipairs_wraps_at_maxinteger() {
    assert_eq!(
        eval(
            "local t = {[math.mininteger] = 10} \
             local f = ipairs{} \
             local k, v = f(t, math.maxinteger) \
             assert(k == math.mininteger and v == 10) \
             return tostring(f(t, k))"
        ),
        "nil"
    );
}

#[test]
fn table_insert_respects_len_metamethod() {
    // `table.insert` lives in the Lua prelude so `#t` and `t[k] = v` go
    // through __len/__newindex; maxinteger + 1 wraps like PUC.
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__len = function() return math.maxinteger end}) \
             table.insert(t, 20) \
             local k, v = next(t) \
             return k .. ':' .. v"
        ),
        "-9223372036854775808:20"
    );
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__len = function() return 2 end}) \
             table.insert(t, 5) \
             return t[3]"
        ),
        "5"
    );
    assert_eq!(
        eval(
            "local t = setmetatable({}, {__len = function() return 2 end, \
              __newindex = function(t, k, v) rawset(t, k, v) end}) \
             table.insert(t, 1, 5) \
             return t[1]"
        ),
        "5"
    );
    assert!(run_err("table.insert({}, 0, 1)").contains("position out of bounds"));
    assert!(run_err("table.insert({}, 1, 2, 3)").contains("wrong number of arguments"));
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

#[test]
fn table_move() {
    // forward, overlapping, backward, and explicit-destination forms
    assert_eq!(
        eval("local a = {10,20,30} table.move(a,1,3,2) return table.concat(a, ',')"),
        "10,10,20,30"
    );
    assert_eq!(
        eval("local a = {10,20,30} table.move(a,1,3,3) return table.concat(a, ',')"),
        "10,20,10,20,30"
    );
    assert_eq!(
        eval("local a = {10,20,30} table.move(a,2,3,1) return table.concat(a, ',')"),
        "20,30,30"
    );
    assert_eq!(
        eval(
            "local a = {} \
             assert(table.move({10,20,30}, 1, 3, 1, a) == a) \
             return table.concat(a, ',')"
        ),
        "10,20,30"
    );
    // empty range and same-place move leave the table alone
    assert_eq!(
        eval(
            "local a = {1,2,3} \
             assert(table.move({10,20,30}, 1, 0, 3, a) == a) \
             table.move(a, 1, 10, 1) \
             return table.concat(a, ',')"
        ),
        "1,2,3"
    );
    // fringes of the integer range
    assert_eq!(
        eval(
            "local a = table.move({[math.maxinteger] = 100}, math.maxinteger, \
             math.maxinteger, math.mininteger) \
             return a[math.mininteger]"
        ),
        "100"
    );
    // bounds / overflow errors
    assert!(run_err("table.move(1, 2, 3, 4)").contains("table expected"));
    assert!(run_err("table.move({}, 1, math.maxinteger, 2)").contains("wrap around"));
    assert!(run_err("table.move({}, math.mininteger, math.maxinteger, 1)").contains("too many"));
}

#[test]
fn table_move_respects_metamethods() {
    // __index on the source, __newindex never touched (writes go to the dest)
    assert_eq!(
        eval(
            "local a = setmetatable({}, {__index = function(_, k) return k * 10 end, \
                                        __newindex = error}) \
             local b = table.move(a, 1, 10, 3, {}) \
             return b[3] .. ',' .. b[12]"
        ),
        "10,100"
    );
    // overlapping move into the source itself is done backwards
    assert_eq!(
        eval(
            "local t = {1, 2, 3} \
             local p = setmetatable({}, {__len = function() return #t end, \
                                        __index = t, __newindex = t}) \
             table.move(p, 1, 3, 2) \
             return table.concat(t, ',')"
        ),
        "1,1,2,3"
    );
}

#[test]
fn table_remove_concat_unpack_respect_metamethods() {
    // remove drives __len/__index/__newindex
    assert_eq!(
        eval(
            "local t = {1, 2, 3} \
             local p = setmetatable({}, {__len = function() return #t end, \
                                        __index = t, __newindex = t}) \
             local v = table.remove(p, 1) \
             return v .. ':' .. #t .. ':' .. table.concat(t, ',')"
        ),
        "1:2:2,3"
    );
    // concat drives __len/__index
    assert_eq!(
        eval(
            "local p = setmetatable({}, {__len = function() return 5 end, \
                                        __index = function(_, k) return k + 1 end}) \
             return table.concat(p, ';')"
        ),
        "2;3;4;5;6"
    );
    // unpack drives __len/__index
    assert_eq!(
        eval(
            "local t = {9, 10} \
             local p = setmetatable({}, {__len = function() return #t end, __index = t}) \
             local a, b, c = table.unpack(p) \
             return a .. ',' .. b .. ',' .. tostring(c)"
        ),
        "9,10,nil"
    );
    // plain tables keep the raw fast-path behavior
    assert_eq!(eval("return table.concat({1, 2, 3}, '-')"), "1-2-3");
    assert_eq!(eval("return tostring(table.remove({}))"), "nil");
    assert!(run_err("table.remove({1, 2}, 0)").contains("position out of bounds"));
    // border-0 element is returned and cleared, as in PUC
    assert_eq!(
        eval("local a = {[0] = 'ban'} return table.remove(a)"),
        "ban"
    );
    assert_eq!(
        eval("local a = {[0] = 'ban'} table.remove(a) return tostring(a[0])"),
        "nil"
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
    assert_eq!(
        eval("return math.type(1) .. '/' .. math.type(1.0)"),
        "integer/float"
    );
    assert_eq!(eval("return tostring(math.type('x'))"), "nil");
    assert_eq!(eval("return math.huge > 1e308"), "true");
    assert_eq!(eval("return math.maxinteger"), i64::MAX.to_string());
    assert_eq!(eval("return math.ult(-1, 0)"), "false"); // -1 as unsigned is huge
    assert_eq!(eval("return math.log(8, 2)"), "3.0");
    assert!(eval("return math.sin(0)") == "0.0");
    assert_eq!(
        eval("return math.abs(math.deg(math.pi) - 180) < 1e-9"),
        "true"
    );
    assert_eq!(
        eval("return math.abs(math.rad(180) - math.pi) < 1e-9"),
        "true"
    );
    assert_eq!(eval("return math.deg(0)"), "0.0");
    assert_eq!(eval("return math.rad(0)"), "0.0");
}

#[test]
fn integer_representation_errors() {
    // PUC 5.4: these are runtime errors (the chunk loads fine), matching
    // math.lua's `checkcompt` helper which pcalls the loaded function.
    assert_eq!(eval("return type(load('return 2 // 0'))"), "function");
    assert!(run_err("return (load('return 2 // 0'))()").contains("attempt to divide by zero"));
    assert!(run_err("return 2.3 >> 0").contains("has no integer representation"));
    assert!(run_err("return 2.3 ~ 0.0").contains("has no integer representation"));
    assert!(run_err("return 1 | 2.0^63").contains("has no integer representation"));
    // PUC names the offending operand's provenance (varinfo).
    assert!(run_err("return math.huge << 1").contains("field 'huge'"));
    assert!(run_err("return math.huge | math.huge").contains("field 'huge'"));
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
