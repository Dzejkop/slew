//! Phase 4: the `utf8` library, the `%z` pattern class, and `%p` formatting.

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
            Err(e) => panic!("{e}\nsource:\n{src}"),
        }
    }
    panic!("script did not finish: {src}");
}

/// Evaluates and renders the first result (like tests/stdlib.rs).
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

/// Asserts the script raises an error whose message contains `needle`.
fn assert_error(source: &str, needle: &str) {
    let src = format!(
        "local ok, err = pcall(function() {source} end)\n\
         assert(not ok, 'expected failure')\n\
         return err"
    );
    let msg = eval(&src);
    assert!(
        msg.contains(needle),
        "error {msg:?} does not contain {needle:?}"
    );
}

// ---- utf8 ----

#[test]
fn charpattern_is_the_puc_byte_class() {
    assert_eq!(eval("return #utf8.charpattern"), "14");
    // the exact bytes: [\0-\x7F\xC2-\xFD][\x80-\xBF]*
    let expected: Vec<u8> = b"[\x00-\x7F\xC2-\xFD][\x80-\xBF]*".to_vec();
    let mut lua = Lua::new();
    let vals = run(&mut lua, "return utf8.charpattern");
    assert_eq!(lua.str_bytes(vals[0]).unwrap(), &expected[..]);
    // it matches a single well-formed character
    assert_eq!(
        eval("return string.match('\\xC3\\xA1', '^' .. utf8.charpattern .. '$') == '\\xC3\\xA1'"),
        "true"
    );
}

#[test]
fn char_and_codepoint() {
    assert_eq!(eval("return utf8.char()"), "");
    assert_eq!(eval("return utf8.char(0, 97, 98, 99, 1)"), "\0abc\x01");
    assert_eq!(
        eval("return utf8.codepoint(utf8.char(0x10FFFF))"),
        "1114111"
    );
    assert_eq!(eval("return utf8.codepoint('\\xC3\\xA1', 1, 1)"), "225");
    // multiple codepoints from a range
    assert_eq!(
        eval_multi("return utf8.codepoint('\\xE6\\xB1\\x89', 1, -1)"),
        ["27721"]
    );
    assert_eq!(
        eval_multi("return utf8.codepoint('\\xE6\\xB1\\x89\\xE5\\xAD\\x97', 1, -1)"),
        ["27721", "23383"]
    );
    // lax accepts surrogates and values above 0x10FFFF
    assert_eq!(
        eval("return utf8.codepoint('\\xED\\xA0\\x80', 1, 1, true)"),
        "55296"
    );
    // empty interval yields no values
    assert!(eval_multi("return utf8.codepoint('abc', 4, 3)").is_empty());
}

#[test]
fn char_and_codepoint_errors() {
    assert_error("utf8.char(-1)", "value out of range");
    assert_error("utf8.char(0x80000000)", "value out of range");
    assert_error(
        "utf8.codepoint('\\xF4\\x9F\\xBF\\xBF')",
        "invalid UTF-8 code",
    );
    assert_error("utf8.codepoint('abc', 0)", "out of bounds");
    assert_error("utf8.codepoint('abc', 1, 4)", "out of bounds");
    assert_error("utf8.codepoint('\\xED\\xA0\\x80')", "invalid UTF-8 code"); // surrogate
    assert_error("utf8.codepoint('\\xC0\\x80')", "invalid UTF-8 code"); // overlong
}

#[test]
fn len_counts_and_reports_malformed() {
    assert_eq!(eval("return utf8.len('汉字/漢字')"), "5");
    assert_eq!(eval("return utf8.len('hello World')"), "11");
    assert_eq!(eval("return utf8.len('\\xE6\\xB1\\x89', 1, -1)"), "1");
    // malformed: nil + the offending 1-based byte position
    assert_eq!(eval_multi("return utf8.len('abc\\xE3def')"), ["nil", "4"]);
    assert_eq!(
        eval_multi("return utf8.len('\\xF4\\x9F\\xBF')"),
        ["nil", "1"]
    );
    assert_eq!(eval_multi("return utf8.len('hel\\x80lo')"), ["nil", "4"]);
    // out-of-bounds indices are errors
    assert_error("utf8.len('abc', 0, 2)", "out of bounds");
    assert_error("utf8.len('abc', 1, 4)", "out of bounds");
}

#[test]
fn offset_finds_character_starts() {
    assert_eq!(eval("return utf8.offset('ábl', 1)"), "1");
    assert_eq!(eval("return utf8.offset('ábl', 2)"), "3");
    assert_eq!(eval("return utf8.offset('ábl', 0)"), "1");
    // byte 2 is inside 'á': offset 0 snaps back to its start
    assert_eq!(eval("return utf8.offset('ábl', 0, 2)"), "1");
    assert_eq!(eval("return utf8.offset('ábl', -1, 4)"), "3");
    // no such character
    assert_eq!(eval("return utf8.offset('alo', 5) == nil"), "true");
    assert_eq!(eval("return utf8.offset('alo', -4) == nil"), "true");
    assert_error("utf8.offset('abc', 1, 5)", "position out of bounds");
    assert_error("utf8.offset('abc', 1, -4)", "position out of bounds");
    assert_error("utf8.offset('\\x80', 1)", "continuation byte");
}

#[test]
fn codes_iterates_codepoints() {
    let src = "local t = {}\n\
               for p, c in utf8.codes('áéí') do t[#t+1] = p .. ':' .. c end\n\
               return table.concat(t, ',')";
    assert_eq!(eval(src), "1:225,3:233,5:237");
    // empty string: the iterator yields nothing
    assert_eq!(
        eval("local f = utf8.codes('') return f('', 2) == nil"),
        "true"
    );
    assert_eq!(
        eval("local f = utf8.codes('') return f('', -1) == nil"),
        "true"
    );
}

#[test]
fn codes_rejects_malformed() {
    assert_error(
        "for c in utf8.codes('ab\\xff') do end",
        "invalid UTF-8 code",
    );
    assert_error(
        "for c in utf8.codes('in\\x80valid') do end",
        "invalid UTF-8 code",
    );
    assert_error("utf8.codes('\\x80')", "invalid UTF-8 code");
}

#[test]
fn works_via_require() {
    // `require 'utf8'` must resolve through package.loaded
    assert_eq!(eval("local u = require 'utf8' return u == utf8"), "true");
}

// ---- pattern classes ----

#[test]
fn deprecated_z_class() {
    // `%z` matches only the zero byte; `%Z` is its complement.
    assert_eq!(eval("return string.match('a\\0b', '%z') == '\\0'"), "true");
    assert_eq!(eval("return #string.match('abc', '%Z*')"), "3");
    assert_eq!(
        eval("local n = 0 for _ in string.gmatch('\\0a\\0b', '%z') do n = n + 1 end return n"),
        "2"
    );
    assert_eq!(eval("return string.match('ab', '%z') == nil"), "true");
}

#[test]
fn all_pattern_classes_cover_ascii() {
    // each class (and complement) exercised against a representative byte
    for (pat, input, hit) in [
        ("%a", "a", "true"),
        ("%A", "a", "false"),
        ("%c", "\x01", "true"),
        ("%C", "\x01", "false"),
        ("%d", "5", "true"),
        ("%g", "!", "true"),
        ("%l", "a", "true"),
        ("%L", "a", "false"),
        ("%p", ".", "true"),
        ("%P", ".", "false"),
        ("%s", " ", "true"),
        ("%S", " ", "false"),
        ("%u", "A", "true"),
        ("%U", "A", "false"),
        ("%w", "_", "false"),
        ("%W", "_", "true"),
        ("%x", "f", "true"),
        ("%X", "f", "false"),
    ] {
        let src = format!("return string.match('{input}', '{pat}') ~= nil");
        assert_eq!(eval(&src), hit, "class {pat} on {input:?}");
    }
}

#[test]
fn bracket_classes_and_anchors() {
    assert_eq!(eval("return #string.match('abc123', '[^%d]+')"), "3");
    assert_eq!(eval("return string.match('x]y', '[%]]')"), "]");
    assert_eq!(eval("return string.match('a-b', '[%-]')"), "-");
    assert_eq!(eval("return #string.match('aaa', '^a+$')"), "3");
    assert_eq!(eval("return string.find('baaa', '^a') == nil"), "true");
}

// ---- string.format %p ----

#[test]
fn format_pointer() {
    assert_eq!(eval("return string.format('%p', 4)"), "(null)");
    assert_eq!(eval("return string.format('%p', true)"), "(null)");
    assert_eq!(eval("return string.format('%p', nil)"), "(null)");
    assert_eq!(eval("return string.format('%p', {}) ~= '(null)'"), "true");
    assert_eq!(
        eval("return string.format('%p', print) ~= '(null)'"),
        "true"
    );
    // equal strings share an identity; distinct tables do not
    assert_eq!(
        eval("local s = 'x' local r = 'x' return string.format('%p', s) == string.format('%p', r)"),
        "true"
    );
    assert_eq!(
        eval("return string.format('%p', {}) ~= string.format('%p', {})"),
        "true"
    );
    assert_eq!(eval("return #string.format('%90p', {})"), "90");
    assert_eq!(eval("return #string.format('%-60p', {})"), "60");
    assert_eq!(
        eval("return string.format('%10p', false) == string.rep(' ', 10 - 6) .. '(null)'"),
        "true"
    );
}

// ---- empty-match semantics (PUC's `lastmatch`) and gmatch `init` ----

#[test]
fn gsub_rejects_empty_match_after_previous() {
    assert_eq!(eval("return string.gsub('a b cd', ' *', '-')"), "-a-b-c-d-");
    assert_eq!(eval("return string.gsub('', '^', 'r')"), "r");
    assert_eq!(eval("return string.gsub('', '$', 'r')"), "r");
    assert_eq!(eval("return string.gsub('aaa', 'a*', 'x')"), "x");
    assert_eq!(eval("return string.gsub('abc', 'x*', '-')"), "-a-b-c-");
    // no substitution: the result is unchanged
    assert_eq!(
        eval("return (string.gsub('abc', 'x*', function() return nil end))"),
        "abc"
    );
}

#[test]
fn gmatch_empty_match_and_init() {
    let src = "local res='' local sub='a  \\nbc\\t\\td' local i=1 \
               for p,e in string.gmatch(sub, '()%s*()') do \
                 res=res..string.sub(sub,i,p-1)..'-' i=e end return res";
    assert_eq!(eval(src), "-a-b-c-d-");
    assert_eq!(
        eval(
            "local s=0 for k in string.gmatch('10 20 30','%d+',3) do s=s+tonumber(k) end return s"
        ),
        "50"
    );
    assert_eq!(
        eval(
            "local s=0 for k in string.gmatch('11 21 31','%d+',-4) do s=s+tonumber(k) end return s"
        ),
        "32"
    );
    // an empty string at the end is matched once; nothing beyond it
    assert_eq!(
        eval("local s=0 for k in string.gmatch('11 21 31','%w*',9) do s=s+1 end return s"),
        "1"
    );
    assert_eq!(
        eval("local s=0 for k in string.gmatch('11 21 31','%w*',10) do s=s+1 end return s"),
        "0"
    );
    assert_error("string.gmatch('x', '%a', 1.5)", "integer representation");
}
