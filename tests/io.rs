//! Regression tests for the host-capability `io` library and the userdata
//! value type.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use slew::{DateParts, Host, HostError, Lua, SeekWhence, StdHost, Step, Value};

fn with_host() -> Lua {
    let mut lua = Lua::new();
    let root = std::env::temp_dir();
    lua.set_host(StdHost::new(root));
    lua
}

fn run(lua: &mut Lua, src: &str) -> Vec<Value> {
    let chunk = lua
        .load(src)
        .unwrap_or_else(|e| panic!("{e}\nsource:\n{src}"));
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

fn run_err(lua: &mut Lua, src: &str) -> String {
    let chunk = lua.load(src).unwrap();
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(_)) => panic!("expected error: {src}"),
            Ok(Step::Pending) => continue,
            Err(e) => return e.to_string(),
        }
    }
}

#[test]
fn io_os_absent_without_host() {
    let mut lua = Lua::new();
    assert!(!lua.has_host());
    assert_eq!(lua.get_global("io"), Value::Nil);
    assert_eq!(lua.get_global("os"), Value::Nil);
    // require cannot conjure a library that does not exist
    let vals = run(
        &mut lua,
        r#"
        assert(io == nil)
        assert(os == nil)
        local ok, err = pcall(require, "io")
        assert(not ok and type(err) == "string")
        local ok2 = pcall(require, "os")
        assert(not ok2)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn clear_host_removes_libraries() {
    let mut lua = with_host();
    assert!(lua.has_host());
    assert_ne!(lua.get_global("io"), Value::Nil);
    lua.clear_host();
    assert!(!lua.has_host());
    assert_eq!(lua.get_global("io"), Value::Nil);
    assert_eq!(lua.get_global("os"), Value::Nil);
    let vals = run(
        &mut lua,
        r#"local ok = pcall(require, "io") assert(not ok) return true"#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn require_io_os_when_host_installed() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        assert(require("io") == io)
        assert(require("os") == os)
        assert(type(io.stdin) == "userdata")
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn userdata_semantics() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        -- type, rawlen error, %p stability/distinctness
        assert(type(io.stdin) == "userdata")
        assert(not pcall(rawlen, io.stdin))
        assert(string.format("%p", io.stdin) ~= string.format("%p", nil))
        assert(string.format("%p", io.stdin) == string.format("%p", io.stdin))
        assert(string.format("%p", io.stdin) ~= string.format("%p", io.stdout))
        -- tostring default
        local s = tostring(io.stdin)
        assert(type(s) == "string" and #s > 0)
        -- valid, distinct table key
        local t = {}
        t[io.stdin] = 1
        t[io.stdout] = 2
        assert(t[io.stdin] == 1 and t[io.stdout] == 2)
        -- getmetatable honors the file metatable, setmetatable refuses non-tables
        assert(type(getmetatable(io.stdin)) == "table")
        -- identity equality
        assert(io.stdin == io.stdin)
        assert(io.stdin ~= io.stdout)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn file_round_trip_read_formats_seek_lines() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        local f = assert(io.open(name, "w"))
        assert(io.type(f) == "file")
        f:write("line1\nline2\n123 456\n")
        f:flush()
        f:close()
        assert(io.type(f) == "closed file")

        local g = assert(io.open(name, "r"))
        assert(g:read("l") == "line1")
        assert(g:read("L") == "line2\n")
        assert(g:read("n") == 123)
        assert(g:read("n") == 456)
        local pos = g:seek("set", 0)
        assert(pos == 0)
        assert(g:read(5) == "line1")
        assert(g:read(0) == "")
        g:seek("set", 0)
        local lines = {}
        for line in g:lines() do lines[#lines + 1] = line end
        assert(#lines == 3 and lines[1] == "line1" and lines[3] == "123 456")
        g:close()

        local via_io = {}
        for line in io.lines(name) do via_io[#via_io + 1] = line end
        assert(#via_io == 3 and via_io[2] == "line2")

        assert(os.remove(name) == true)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn io_write_goes_to_captured_sinks() {
    let mut lua = Lua::new();
    let host = StdHost::new(std::env::temp_dir());
    let out: Rc<RefCell<Vec<u8>>> = host.stdout_sink();
    let err: Rc<RefCell<Vec<u8>>> = host.stderr_sink();
    lua.set_host(host);
    let vals = run(
        &mut lua,
        r#"
        io.write("hello ", "world\n")
        io.stderr:write("oops\n")
        io.stdout:write("again\n")
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
    assert_eq!(&*out.borrow(), b"hello world\nagain\n");
    assert_eq!(&*err.borrow(), b"oops\n");
}

#[test]
fn stdin_reading() {
    let mut lua = Lua::new();
    let host = StdHost::new(std::env::temp_dir()).with_stdin(b"alpha\nbeta\n42\n".to_vec());
    lua.set_host(host);
    let vals = run(
        &mut lua,
        r#"
        assert(io.read("l") == "alpha")
        assert(io.read("L") == "beta\n")
        assert(io.read("n") == 42)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn io_input_output_switching() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        io.output(name)
        io.write("switched\n")
        io.close(io.output())
        io.input(name)
        assert(io.read("l") == "switched")
        io.close(io.input())
        os.remove(name)
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn closed_file_use_errors() {
    let mut lua = with_host();
    let name = format!("slew_closed_test_{}", std::process::id());
    let src = format!(
        r#"
        local f = assert(io.open("{name}", "w"))
        f:close()
        f:write("x")
        "#
    );
    let msg = run_err(&mut lua, &src);
    let _ = std::fs::remove_file(std::env::temp_dir().join(&name));
    assert!(msg.contains("closed file"), "unexpected message: {msg}");
}

#[test]
fn io_open_missing_returns_nil_msg_errno() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local f, msg, errno = io.open("definitely_missing_slew_file_xyz", "r")
        assert(f == nil and type(msg) == "string" and type(errno) == "number")
        return true
        "#,
    );
    assert_eq!(vals[0], Value::Bool(true));
}

#[test]
fn long_line_spans_read_buffer() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        local f = assert(io.open(name, "w"))
        local long = string.rep("x", 20000)
        f:write(long, "\n")
        f:close()
        local g = assert(io.open(name, "r"))
        local l = g:read("l")
        assert(#l == 20000)
        -- everything after the freed line is a single newline
        assert(g:read("a") == "")
        g:close()
        os.remove(name)
        return #l
        "#,
    );
    assert_eq!(vals[0], Value::Int(20000));
}

#[test]
fn invalid_open_mode_raises() {
    let mut lua = with_host();
    let msg = run_err(&mut lua, r#"io.open("x", "z")"#);
    assert!(msg.contains("invalid mode"), "got: {msg}");
}

/// A number token that runs past the 8192-byte read buffer must not panic:
/// `read("*n")` parses the longest valid prefix and leaves the rest unread.
#[test]
fn read_number_token_spans_read_buffer() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        local f = assert(io.open(name, "w"))
        f:write("123", string.rep("a", 9000))
        f:close()
        local g = assert(io.open(name, "r"))
        local n = g:read("*n")
        local rest = g:read("a")
        g:close()
        os.remove(name)
        return n, #rest
        "#,
    );
    assert_eq!(vals[0], Value::Int(123));
    assert_eq!(vals[1], Value::Int(9000));
}

/// A numeral-shaped prefix with a bad tail is rejected and the tail stays
/// unread, matching PUC (`"12e+"` consumes the prefix but is not a number).
#[test]
fn read_number_rejects_bad_tail_without_consuming_it() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        local f = assert(io.open(name, "w"))
        f:write("12e+Z")
        f:close()
        local g = assert(io.open(name, "r"))
        local n = g:read("*n")
        local rest = g:read("a")
        g:close()
        os.remove(name)
        return n, rest
        "#,
    );
    assert_eq!(vals[0], Value::Nil);
    assert_eq!(lua.display_value(vals[1]), "Z");
}

/// A huge numeric read count must raise a catchable "not enough memory" error,
/// never abort the process via an infallible allocation.
#[test]
fn huge_read_count_is_catchable_error() {
    let mut lua = with_host();
    let vals = run(
        &mut lua,
        r#"
        local name = os.tmpname()
        local f = assert(io.open(name, "w"))
        f:write("abc")
        f:close()
        local g = assert(io.open(name, "r"))
        local ok, err = pcall(g.read, g, math.maxinteger)
        g:close()
        os.remove(name)
        return ok, err
        "#,
    );
    assert_eq!(vals[0], Value::Bool(false));
    let msg = lua.display_value(vals[1]);
    assert!(msg.contains("not enough memory"), "got: {msg}");
}

#[test]
fn rawlen_userdata_error_has_argument_context() {
    let mut lua = with_host();
    let msg = run_err(&mut lua, r#"return rawlen(io.stdin)"#);
    assert!(
        msg.contains("bad argument #1 to 'rawlen'") && msg.contains("table or string expected"),
        "got: {msg}"
    );
}

/// Minimal host that counts `open`/`close` calls, so the test can observe
/// whether GC releases dropped file handles. File contents are irrelevant.
#[derive(Default)]
struct CountingHost {
    opens: Rc<Cell<usize>>,
    closes: Rc<Cell<usize>>,
}

impl Host for CountingHost {
    fn stdout_write(&mut self, _bytes: &[u8]) -> Result<(), HostError> {
        Ok(())
    }
    fn stderr_write(&mut self, _bytes: &[u8]) -> Result<(), HostError> {
        Ok(())
    }
    fn stdin_read(&mut self, _buf: &mut [u8]) -> Result<usize, HostError> {
        Ok(0)
    }
    fn open(&mut self, _path: &str, _mode: &str) -> Result<u64, HostError> {
        let h = self.opens.get() + 1;
        self.opens.set(h);
        Ok(h as u64)
    }
    fn close(&mut self, _handle: u64) -> Result<(), HostError> {
        self.closes.set(self.closes.get() + 1);
        Ok(())
    }
    fn read(&mut self, _handle: u64, _buf: &mut [u8]) -> Result<usize, HostError> {
        Ok(0)
    }
    fn write(&mut self, _handle: u64, _bytes: &[u8]) -> Result<usize, HostError> {
        Ok(0)
    }
    fn seek(&mut self, _handle: u64, _whence: SeekWhence, _offset: i64) -> Result<u64, HostError> {
        Ok(0)
    }
    fn flush(&mut self, _handle: u64) -> Result<(), HostError> {
        Ok(())
    }
    fn remove(&mut self, _path: &str) -> Result<(), HostError> {
        Ok(())
    }
    fn rename(&mut self, _from: &str, _to: &str) -> Result<(), HostError> {
        Ok(())
    }
    fn tmpname(&mut self) -> Result<String, HostError> {
        Ok("slew_mock_tmp".to_string())
    }
    fn getenv(&mut self, _name: &str) -> Option<Vec<u8>> {
        None
    }
    fn clock(&mut self) -> f64 {
        0.0
    }
    fn time(&mut self) -> i64 {
        0
    }
    fn time_parts(&mut self, _t: i64, _utc: bool) -> DateParts {
        DateParts {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            min: 0,
            sec: 0,
            wday: 4,
            yday: 1,
            isdst: false,
        }
    }
    fn make_time(&mut self, _parts: DateParts) -> i64 {
        0
    }
    fn setlocale(&mut self, _locale: Option<&str>, _category: Option<&str>) -> Option<String> {
        Some("C".to_string())
    }
}

/// Dropped file userdata carry a *native* `__gc`; the finalizer driver must
/// drain those like Lua-closure finalizers instead of stalling after the first
/// one and permanently rooting `pending_finalizers`.
#[test]
fn native_file_finalizers_drain_on_collectgarbage() {
    const N: usize = 50;
    let mut lua = Lua::new();
    let host = CountingHost::default();
    let closes = host.closes.clone();
    lua.set_host(host);

    let src = format!(
        r#"
        local function make()
          local t = {{}}
          for i = 1, {N} do t[i] = assert(io.open('mock_' .. i, 'w')) end
        end
        make()
        for _ = 1, {N} * 2 do collectgarbage() end
        "#
    );
    run(&mut lua, &src);
    assert!(
        closes.get() >= N - 1,
        "native __gc finalizers did not drain: closed {} of {N} dropped files",
        closes.get()
    );
}
