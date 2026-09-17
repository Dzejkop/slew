//! Regression tests for the host-capability `os` library.

use slew::{Lua, StdHost, Step, Value};
use test_case::test_case;

fn with_host() -> Lua {
    let mut lua = Lua::new();
    lua.set_host(StdHost::new(std::env::temp_dir()));
    lua
}

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua.load(src).unwrap_or_else(|e| panic!("{e}\n{src}"));
    let mut exec = lua.execute(&chunk);
    for _ in 0..100_000 {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(vals)) => return vals,
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
            Err(e) => panic!("{e}\n{src}"),
        }
    }
    panic!("did not finish: {src}");
}

#[test]
fn clock_time_date_difftime() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        assert(type(os.clock()) == "number")
        assert(os.clock() >= 0.0)
        local now = os.time()
        assert(type(now) == "number" and now > 1600000000)
        assert(os.difftime(now, now - 10) == 10.0)
        assert(os.difftime(now, now) == 0.0)

        -- deterministic date formatting on a fixed timestamp
        local t = 0  -- 1970-01-01T00:00:00Z
        assert(os.date("!%Y-%m-%d", t) == "1970-01-01")
        assert(os.date("!%H:%M:%S", t) == "00:00:00")
        assert(os.date("!%Y", t) == "1970")
        assert(os.date("!%j", t) == "001")
        -- round-trip through the table form
        local d = os.date("!*t", t)
        assert(d.year == 1970 and d.month == 1 and d.day == 1)
        assert(d.hour == 0 and d.min == 0 and d.sec == 0)
        assert(d.wday == 5)  -- 1970-01-01 was a Thursday; 1=Sunday..5=Thursday
        assert(d.yday == 1)
        assert(os.time{year=1970, month=1, day=1, hour=0, min=0, sec=0} == 0)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

// `os.difftime` coerces integral floats and numeric strings for both
// arguments.
#[test_case("os.difftime(\"10\", 5)", "5.0"; "numeric_string_coerces")]
#[test_case("os.difftime(10.0, 3.0)", "7.0"; "integral_floats_coerce")]
fn difftime_coerces_numbers(call: &str, expected: &str) {
    let mut lua = with_host();
    let vals = run(&mut lua, &format!("return {call} == {expected}"));
    assert_eq!(vals[0], Value::Bool(true), "{call} should equal {expected}");
}

#[test_case("os.difftime(10)" ; "missing_second_argument")]
#[test_case("os.difftime(10, nil)" ; "nil_second_argument")]
#[test_case("os.difftime(1.5, 0)" ; "fractional_argument")]
#[test_case("os.difftime({})" ; "table_argument")]
fn difftime_rejects_bad_arguments(call: &str) {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        &format!("return not pcall(function() return {call} end)"),
    );
    assert_eq!(vals[0], Value::Bool(true), "{call} should error");
}

#[test]
fn date_preserves_non_utf8_format_bytes() {
    // Lua strings are byte strings: literal bytes in the format survive.
    let mut lua = with_host();
    let vals = run(&mut lua, r#"return os.date("\xff%Y", 0)"#);
    assert_eq!(
        lua.str_bytes(vals[0]),
        Some(&b"\xff1970"[..]),
        "os.date must not re-encode literal bytes"
    );
}

#[test]
fn tmpname_remove_rename() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local a = os.tmpname()
        local b = os.tmpname()
        assert(type(a) == "string" and a ~= b)
        local f = assert(io.open(a, "w"))
        f:write("data")
        f:close()
        assert(os.rename(a, b) == true)
        assert(os.remove(b) == true)
        -- removing a missing file yields nil, message, errno
        local ok, msg, errno = os.remove(b)
        assert(ok == nil and type(msg) == "string" and type(errno) == "number")
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn getenv_and_setlocale() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        assert(type(os.getenv("PATH")) == "string")
        assert(os.getenv("SLEW_SURELY_UNSET_VAR_12345") == nil)
        assert(os.setlocale() == "C")
        assert(os.setlocale(nil, "numeric") == "C")
        assert(os.setlocale("C") == "C")
        assert(os.setlocale("POSIX") == "C")
        assert(os.setlocale("definitely_not_a_locale") == nil)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn exit_is_a_controlled_request() {
    let mut lua = with_host();
    let chunk = lua.load("os.exit(3)").unwrap();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(&mut lua, 1_000_000) {
            Ok(Step::Done(_)) => panic!("os.exit should raise"),
            Ok(Step::Pending) => {}
            Ok(Step::Waiting(_)) => panic!("unexpected native wait"),
            Err(_) => break,
        }
    }
    assert_eq!(lua.take_exit_request(), Some(3));
    assert_eq!(lua.take_exit_request(), None);
}

#[test]
fn os_execute_absent() {
    let mut lua = with_host();
    let vals = run(&mut lua, "return os.execute == nil");
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn os_absent_without_host() {
    let lua = Lua::<()>::new();
    assert_eq!(lua.get_global("os"), Value::Nil);
}
