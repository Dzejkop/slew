//! The `io` library, backed entirely by the embedder-installed [`Host`].
//!
//! Nothing here touches the operating system directly: file handles are
//! host-owned ids inside userdata, and standard streams go through the host's
//! `stdin`/`stdout`/`stderr` methods. When no host is installed these
//! functions do not exist at all.

use crate::host::{HostError, HostObject, SeekWhence, Userdata};
use crate::value::UserdataId;
use crate::value::Value;
use crate::vm::Lua;

use super::{arg, check_bytes, set_field};

/// Builds the shared file userdata metatable and its method table. `lines` is
/// added later by the Lua prelude (it needs to close over the format list).
pub(super) fn build_file_metatable(lua: &mut Lua) -> crate::value::TableId {
    let methods = lua.new_table();
    let natives: [(&str, crate::vm::NativeFn); 7] = [
        ("read", n_read),
        ("write", n_write),
        ("seek", n_seek),
        ("close", n_close),
        ("flush", n_flush),
        ("setvbuf", n_setvbuf),
        ("lines", n_lines_stub),
    ];
    for (name, f) in natives {
        let v = lua.add_native(name, f);
        set_field(lua, methods, name, v);
    }

    let meta = lua.new_table();
    let meta_id = match meta {
        Value::Table(id) => id,
        _ => unreachable!(),
    };
    let name = lua.new_string(b"FILE*");
    set_field(lua, meta, "__name", name);
    set_field(lua, meta, "__index", methods);
    let tostring = lua.add_native("__tostring", n_tostring);
    set_field(lua, meta, "__tostring", tostring);
    let gc = lua.add_native("__gc", n_gc);
    set_field(lua, meta, "__gc", gc);
    let close = lua.add_native("__close", n_gc);
    set_field(lua, meta, "__close", close);
    // Hand the methods table to the Lua prelude, which adds `lines`.
    lua.set_global("__slew_file_methods", methods);
    meta_id
}

pub(super) fn install(lua: &mut Lua) {
    let file_meta = lua.file_meta.expect("file metatable installed first");
    let io = lua.new_table();
    lua.set_global("io", io);

    let stdin = new_file(lua, HostObject::Stdin, file_meta);
    let stdout = new_file(lua, HostObject::Stdout, file_meta);
    let stderr = new_file(lua, HostObject::Stderr, file_meta);
    set_field(lua, io, "stdin", stdin);
    set_field(lua, io, "stdout", stdout);
    set_field(lua, io, "stderr", stderr);
    let Value::Userdata(stdin_id) = stdin else {
        unreachable!()
    };
    let Value::Userdata(stdout_id) = stdout else {
        unreachable!()
    };
    lua.io_input = Some(stdin_id);
    lua.io_output = Some(stdout_id);

    let entries: [(&str, crate::vm::NativeFn); 8] = [
        ("open", n_open),
        ("close", n_io_close),
        ("read", n_io_read),
        ("write", n_io_write),
        ("input", n_io_input),
        ("output", n_io_output),
        ("type", n_type),
        ("tmpfile", n_tmpfile),
    ];
    for (name, f) in entries {
        let v = lua.add_native(name, f);
        set_field(lua, io, name, v);
    }
}

/// Runs the small Lua prelude that defines `io.lines` and `file:lines` in
/// terms of the native `read` (so their iterator closures are real Lua
/// functions, suspendable like everything else).
pub(super) fn run_prelude(lua: &mut Lua) {
    let chunk = match lua.load_named("=io", include_str!("io_prelude.lua")) {
        Ok(c) => c,
        Err(e) => panic!("io prelude must compile: {e}"),
    };
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(lua, 10_000_000) {
            Ok(crate::vm::Step::Done(_)) => break,
            Ok(crate::vm::Step::Pending) => continue,
            Err(e) => panic!("io prelude failed: {e}"),
        }
    }
}

pub(super) fn new_file(lua: &mut Lua, object: HostObject, meta: crate::value::TableId) -> Value {
    let ud = Userdata {
        metatable: Some(meta),
        object,
        closed: false,
        finalized: false,
        read_buf: Vec::new(),
        read_pos: 0,
    };
    Value::Userdata(lua.alloc_userdata(ud))
}

/// `nil, "message", errno` return shape used by `io.open` etc.
fn open_failure(lua: &mut Lua, e: HostError) -> Vec<Value> {
    let msg = lua.new_string(e.message.as_bytes());
    vec![Value::Nil, msg, Value::Int(e.errno as i64)]
}

fn want_file(lua: &Lua, args: &[Value], i: usize, who: &str) -> Result<UserdataId, String> {
    match arg(args, i) {
        Value::Userdata(u) => {
            if lua.userdata[u.0 as usize].closed {
                Err("attempt to use a closed file".into())
            } else {
                Ok(u)
            }
        }
        v => Err(format!(
            "bad argument #{} to '{who}' (FILE* expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

// ---- low-level byte access on a file userdata --------------------------

/// Reads more bytes from the host, compacting any consumed prefix first.
/// Returns the number of new bytes (0 at end of file).
fn fill_more(lua: &mut Lua, uid: UserdataId) -> Result<usize, String> {
    let obj = {
        let ud = &mut lua.userdata[uid.0 as usize];
        if ud.read_pos > 0 {
            ud.read_buf.drain(..ud.read_pos);
            ud.read_pos = 0;
        }
        ud.object
    };
    let mut tmp = [0u8; 8192];
    let n = {
        let Some(h) = lua.host.as_mut() else {
            return Err("io library has no host".into());
        };
        match obj {
            HostObject::Stdin => h.stdin_read(&mut tmp),
            HostObject::File(handle) => h.read(handle, &mut tmp),
            _ => return Ok(0),
        }
        .map_err(|e| e.message)?
    };
    lua.userdata[uid.0 as usize]
        .read_buf
        .extend_from_slice(&tmp[..n]);
    Ok(n)
}

/// Ensures at least one buffered byte is available. Returns `true` when bytes
/// are available, `false` at end of file.
fn refill(lua: &mut Lua, uid: UserdataId) -> Result<bool, String> {
    {
        let ud = &lua.userdata[uid.0 as usize];
        if ud.read_pos < ud.read_buf.len() {
            return Ok(true);
        }
    }
    fill_more(lua, uid).map(|n| n > 0)
}

fn peek(lua: &mut Lua, uid: UserdataId) -> Result<Option<u8>, String> {
    if !refill(lua, uid)? {
        return Ok(None);
    }
    let ud = &lua.userdata[uid.0 as usize];
    Ok(Some(ud.read_buf[ud.read_pos]))
}

fn advance(lua: &mut Lua, uid: UserdataId) {
    lua.userdata[uid.0 as usize].read_pos += 1;
}

fn read_line(
    lua: &mut Lua,
    uid: UserdataId,
    keep_newline: bool,
) -> Result<Option<Vec<u8>>, String> {
    loop {
        {
            let ud = &lua.userdata[uid.0 as usize];
            if let Some(rel) = ud.read_buf[ud.read_pos..].iter().position(|&b| b == b'\n') {
                let start = ud.read_pos;
                let nl = start + rel;
                let line = if keep_newline {
                    ud.read_buf[start..=nl].to_vec()
                } else {
                    ud.read_buf[start..nl].to_vec()
                };
                lua.userdata[uid.0 as usize].read_pos = nl + 1;
                return Ok(Some(line));
            }
        }
        // No newline in the buffer yet: append more. `fill_more` compacts the
        // consumed prefix, so a partial line spanning reads is preserved.
        if fill_more(lua, uid)? == 0 {
            let ud = &mut lua.userdata[uid.0 as usize];
            if ud.read_pos < ud.read_buf.len() {
                let line = ud.read_buf[ud.read_pos..].to_vec();
                ud.read_pos = ud.read_buf.len();
                return Ok(Some(line));
            }
            return Ok(None);
        }
    }
}

fn read_count(lua: &mut Lua, uid: UserdataId, n: usize) -> Result<Option<Vec<u8>>, String> {
    if n == 0 {
        return Ok(peek(lua, uid)?.map(|_| Vec::new()));
    }
    let mut out = Vec::with_capacity(n);
    while out.len() < n {
        let take_from_buf = {
            let ud = &lua.userdata[uid.0 as usize];
            let avail = ud.read_buf.len() - ud.read_pos;
            if avail > 0 {
                Some(avail.min(n - out.len()))
            } else {
                None
            }
        };
        match take_from_buf {
            Some(take) => {
                let ud = &mut lua.userdata[uid.0 as usize];
                out.extend_from_slice(&ud.read_buf[ud.read_pos..ud.read_pos + take]);
                ud.read_pos += take;
            }
            None => {
                if !refill(lua, uid)? {
                    break;
                }
            }
        }
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(out))
    }
}

fn read_all(lua: &mut Lua, uid: UserdataId) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        {
            let ud = &lua.userdata[uid.0 as usize];
            if ud.read_pos < ud.read_buf.len() {
                out.extend_from_slice(&ud.read_buf[ud.read_pos..]);
                lua.userdata[uid.0 as usize].read_pos = ud.read_buf.len();
            }
        }
        if !refill(lua, uid)? {
            break;
        }
    }
    Ok(out)
}

fn read_number(lua: &mut Lua, uid: UserdataId) -> Result<Option<Value>, String> {
    // skip leading whitespace
    while let Some(b) = peek(lua, uid)? {
        if b.is_ascii_whitespace() {
            advance(lua, uid);
        } else {
            break;
        }
    }
    let mut token: Vec<u8> = Vec::new();
    while let Some(b) = peek(lua, uid)? {
        if b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.') {
            token.push(b);
            advance(lua, uid);
        } else {
            break;
        }
    }
    while !token.is_empty() {
        if let Some(v) = super::parse_number(&token) {
            return Ok(Some(v));
        }
        token.pop();
        lua.userdata[uid.0 as usize].read_pos -= 1;
    }
    Ok(None)
}

/// One `read` format: `Value` result or `None` on failure (EOF).
fn read_one(lua: &mut Lua, uid: UserdataId, fmt: Value) -> Result<Option<Value>, String> {
    match fmt {
        Value::Int(n) => {
            if n < 0 {
                return Err("bad argument to 'read' (invalid format)".into());
            }
            Ok(read_count(lua, uid, n as usize)?.map(|b| lua.new_string(&b)))
        }
        Value::Float(f) => {
            let n = crate::value::float_to_exact_int(f).ok_or_else(|| {
                "bad argument to 'read' (number has no integer representation)".to_string()
            })?;
            if n < 0 {
                return Err("bad argument to 'read' (invalid format)".into());
            }
            Ok(read_count(lua, uid, n as usize)?.map(|b| lua.new_string(&b)))
        }
        Value::Str(id) => {
            let bytes = lua.strings.get(id).to_vec();
            let bytes = bytes.strip_prefix(b"*").unwrap_or(&bytes);
            match bytes.first().copied() {
                Some(b'n') => read_number(lua, uid),
                Some(b'l') => {
                    let line = read_line(lua, uid, false)?;
                    Ok(line.map(|b| lua.new_string(&b)))
                }
                Some(b'L') => {
                    let line = read_line(lua, uid, true)?;
                    Ok(line.map(|b| lua.new_string(&b)))
                }
                Some(b'a') => {
                    let all = read_all(lua, uid)?;
                    Ok(Some(lua.new_string(&all)))
                }
                _ => Err("bad argument to 'read' (invalid format)".into()),
            }
        }
        _ => Err("bad argument to 'read' (invalid format)".into()),
    }
}

fn do_read(lua: &mut Lua, uid: UserdataId, fmts: &[Value]) -> Result<Vec<Value>, String> {
    let mut results = Vec::new();
    if fmts.is_empty() {
        let lfmt = lua.new_string(b"l");
        match read_one(lua, uid, lfmt)? {
            Some(v) => results.push(v),
            None => results.push(Value::Nil),
        }
        return Ok(results);
    }
    for &fmt in fmts {
        match read_one(lua, uid, fmt)? {
            Some(v) => results.push(v),
            None => {
                results.push(Value::Nil);
                break;
            }
        }
    }
    Ok(results)
}

// ---- natives -----------------------------------------------------------

fn n_read(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "read")?;
    let fmts = args.get(1..).unwrap_or(&[]).to_vec();
    do_read(lua, uid, &fmts)
}

fn n_write(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "write")?;
    let mut bytes = Vec::new();
    for (i, &v) in args.iter().enumerate().skip(1) {
        match v {
            Value::Str(id) => bytes.extend_from_slice(lua.strings.get(id)),
            Value::Int(_) | Value::Float(_) => {
                bytes.extend_from_slice(crate::value::fmt_number(v).as_bytes())
            }
            other => {
                return Err(format!(
                    "bad argument #{} to 'write' (string expected, got {})",
                    i + 1,
                    other.type_name()
                ));
            }
        }
    }
    if let Err(e) = write_handle(lua, uid, &bytes) {
        return Ok(open_failure(lua, e));
    }
    Ok(vec![args[0]])
}

fn write_handle(lua: &mut Lua, uid: UserdataId, bytes: &[u8]) -> Result<(), HostError> {
    let obj = lua.userdata[uid.0 as usize].object;
    let Some(h) = lua.host.as_mut() else {
        return Err(HostError::new("io library has no host"));
    };
    match obj {
        HostObject::Stdout => h.stdout_write(bytes),
        HostObject::Stderr => h.stderr_write(bytes),
        HostObject::File(handle) => h.write(handle, bytes).map(|_| ()),
        HostObject::Stdin => Err(HostError::new("cannot write to stdin")),
    }
}

fn seek_whence(bytes: &[u8]) -> Option<SeekWhence> {
    match bytes {
        b"set" => Some(SeekWhence::Set),
        b"cur" => Some(SeekWhence::Cur),
        b"end" => Some(SeekWhence::End),
        _ => None,
    }
}

fn n_seek(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "seek")?;
    let whence_bytes = match arg(args, 1) {
        Value::Nil => None,
        v => Some(check_bytes(lua, &[v], 0, "seek")?),
    };
    let offset = match arg(args, 2) {
        Value::Nil => 0,
        Value::Int(i) => i,
        Value::Float(f) => crate::value::float_to_exact_int(f).ok_or_else(|| {
            "bad argument #3 to 'seek' (number has no integer representation)".to_string()
        })?,
        v => {
            return Err(format!(
                "bad argument #3 to 'seek' (number expected, got {})",
                v.type_name()
            ));
        }
    };
    let whence = match &whence_bytes {
        None => SeekWhence::Cur,
        Some(b) => seek_whence(b)
            .ok_or_else(|| "bad argument #2 to 'seek' (invalid option)".to_string())?,
    };
    // Discard buffered read-ahead and translate a `cur` offset accordingly.
    let unread = {
        let ud = &mut lua.userdata[uid.0 as usize];
        let unread = ud.read_buf.len() - ud.read_pos;
        ud.read_buf.clear();
        ud.read_pos = 0;
        unread as i64
    };
    let offset = if whence == SeekWhence::Cur {
        offset - unread
    } else {
        offset
    };
    let obj = lua.userdata[uid.0 as usize].object;
    let result = match obj {
        HostObject::File(handle) => match lua.host.as_mut() {
            Some(h) => h.seek(handle, whence, offset),
            None => return Ok(open_failure(lua, HostError::new("io library has no host"))),
        },
        _ => return Ok(open_failure(lua, HostError::new("cannot seek this stream"))),
    };
    match result {
        Ok(pos) => Ok(vec![Value::Int(pos as i64)]),
        Err(e) => Ok(open_failure(lua, e)),
    }
}

fn n_close(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "close")?;
    close_handle(lua, uid)?;
    Ok(vec![Value::Bool(true)])
}

fn close_handle(lua: &mut Lua, uid: UserdataId) -> Result<(), String> {
    if lua.userdata[uid.0 as usize].closed {
        return Err("attempt to use a closed file".into());
    }
    let obj = lua.userdata[uid.0 as usize].object;
    lua.userdata[uid.0 as usize].closed = true;
    lua.userdata[uid.0 as usize].read_buf.clear();
    lua.userdata[uid.0 as usize].read_pos = 0;
    if let HostObject::File(handle) = obj
        && let Some(h) = lua.host.as_mut()
    {
        let _ = h.close(handle);
    }
    Ok(())
}

fn n_flush(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "flush")?;
    let obj = lua.userdata[uid.0 as usize].object;
    if let HostObject::File(handle) = obj
        && let Some(h) = lua.host.as_mut()
        && let Err(e) = h.flush(handle)
    {
        return Ok(open_failure(lua, e));
    }
    Ok(vec![args[0]])
}

fn n_setvbuf(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "setvbuf")?;
    let mode = check_bytes(lua, args, 1, "setvbuf")?;
    if !matches!(&mode[..], b"no" | b"full" | b"line") {
        return Err("bad argument #2 to 'setvbuf' (invalid option)".into());
    }
    let size = match arg(args, 2) {
        Value::Nil => 0usize,
        Value::Int(i) if i >= 0 => i as usize,
        _ => return Err("bad argument #3 to 'setvbuf' (number expected)".into()),
    };
    let obj = lua.userdata[uid.0 as usize].object;
    if let HostObject::File(handle) = obj
        && let Some(h) = lua.host.as_mut()
        && let Err(e) = h.setvbuf(handle, &mode, size)
    {
        return Ok(open_failure(lua, e));
    }
    Ok(vec![args[0]])
}

fn n_lines_stub(_lua: &mut Lua, _args: &[Value]) -> Result<Vec<Value>, String> {
    // Replaced by the Lua prelude's `methods:lines`.
    Err("bad argument #1 to 'lines' (FILE* expected)".into())
}

fn n_tostring(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = want_file(lua, args, 0, "tostring")?;
    let s = if lua.userdata[uid.0 as usize].closed {
        "file (closed)".to_string()
    } else {
        format!("file (0x{:08x})", uid.0)
    };
    Ok(vec![lua.new_string(s.as_bytes())])
}

fn n_gc(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    if let Value::Userdata(u) = arg(args, 0)
        && !lua.userdata[u.0 as usize].closed
    {
        let _ = close_handle(lua, u);
    }
    Ok(vec![])
}

// ---- io.* --------------------------------------------------------------

fn valid_mode(mode: &[u8]) -> bool {
    if !matches!(mode.first(), Some(b'r' | b'w' | b'a')) {
        return false;
    }
    let mut plus = false;
    for &b in &mode[1..] {
        match b {
            b'b' => {}
            b'+' if !plus => plus = true,
            _ => return false,
        }
    }
    true
}

fn n_open(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let path = check_bytes(lua, args, 0, "open")?;
    let mode = match arg(args, 1) {
        Value::Nil => b"r".to_vec(),
        v => check_bytes(lua, &[v], 0, "open")?,
    };
    if !valid_mode(&mode) {
        return Err("bad argument #2 to 'open' (invalid mode)".into());
    }
    let path_str = String::from_utf8_lossy(&path).into_owned();
    let mode_str = String::from_utf8_lossy(&mode).into_owned();
    let file_meta = lua.file_meta.expect("file metatable");
    let Some(h) = lua.host.as_mut() else {
        return Ok(open_failure(lua, HostError::new("io library has no host")));
    };
    match h.open(&path_str, &mode_str) {
        Ok(handle) => Ok(vec![new_file(lua, HostObject::File(handle), file_meta)]),
        Err(mut e) => {
            e.message = format!("{path_str}: {}", e.message);
            Ok(open_failure(lua, e))
        }
    }
}

fn default_input(lua: &mut Lua, who: &str) -> Result<UserdataId, String> {
    lua.io_input
        .ok_or_else(|| format!("bad argument #1 to '{who}' (no default input)"))
}

fn default_output(lua: &mut Lua, who: &str) -> Result<UserdataId, String> {
    lua.io_output
        .ok_or_else(|| format!("bad argument #1 to '{who}' (no default output)"))
}

fn n_io_read(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = default_input(lua, "read")?;
    let fmts = args.to_vec();
    do_read(lua, uid, &fmts)
}

fn n_io_write(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = default_output(lua, "write")?;
    let mut bytes = Vec::new();
    for (i, &v) in args.iter().enumerate() {
        match v {
            Value::Str(id) => bytes.extend_from_slice(lua.strings.get(id)),
            Value::Int(_) | Value::Float(_) => {
                bytes.extend_from_slice(crate::value::fmt_number(v).as_bytes())
            }
            other => {
                return Err(format!(
                    "bad argument #{} to 'write' (string expected, got {})",
                    i + 1,
                    other.type_name()
                ));
            }
        }
    }
    if let Err(e) = write_handle(lua, uid, &bytes) {
        return Ok(open_failure(lua, e));
    }
    Ok(vec![Value::Userdata(uid)])
}

fn n_io_close(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let uid = match arg(args, 0) {
        Value::Nil => default_output(lua, "close")?,
        v => match v {
            Value::Userdata(u) => u,
            other => {
                return Err(format!(
                    "bad argument #1 to 'close' (FILE* expected, got {})",
                    other.type_name()
                ));
            }
        },
    };
    if lua.userdata[uid.0 as usize].closed {
        return Err("attempt to use a closed file".into());
    }
    close_handle(lua, uid)?;
    Ok(vec![Value::Bool(true)])
}

/// Opens a named file for `io.input`/`io.output`: `Ok(file)` or `Err(message)`
/// so the wrapper can raise with a PUC-shaped message.
fn open_named(lua: &mut Lua, name: &[u8], mode: &[u8]) -> Result<Value, String> {
    let file_meta = lua.file_meta.expect("file metatable");
    let path = String::from_utf8_lossy(name).into_owned();
    let mode_str = String::from_utf8_lossy(mode).into_owned();
    let opened = {
        let Some(h) = lua.host.as_mut() else {
            return Err(format!("cannot open file '{path}' (no host)"));
        };
        h.open(&path, &mode_str)
    };
    match opened {
        Ok(handle) => Ok(new_file(lua, HostObject::File(handle), file_meta)),
        Err(e) => Err(format!("cannot open file '{path}' ({})", e.message)),
    }
}

fn n_io_input(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    set_default_stream(lua, args, true)
}

fn n_io_output(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    set_default_stream(lua, args, false)
}

fn set_default_stream(lua: &mut Lua, args: &[Value], input: bool) -> Result<Vec<Value>, String> {
    match arg(args, 0) {
        Value::Nil => {
            let uid = if input {
                default_input(lua, "input")?
            } else {
                default_output(lua, "output")?
            };
            Ok(vec![Value::Userdata(uid)])
        }
        Value::Str(id) => {
            let name = lua.strings.get(id).to_vec();
            let mode: &[u8] = if input { b"r" } else { b"w" };
            let f = open_named(lua, &name, mode)?;
            let Value::Userdata(uid) = f else {
                unreachable!()
            };
            if input {
                lua.io_input = Some(uid);
            } else {
                lua.io_output = Some(uid);
            }
            Ok(vec![f])
        }
        Value::Userdata(uid) => {
            if input {
                lua.io_input = Some(uid);
            } else {
                lua.io_output = Some(uid);
            }
            Ok(vec![Value::Userdata(uid)])
        }
        other => Err(format!(
            "bad argument #1 to '{}' (string expected, got {})",
            if input { "input" } else { "output" },
            other.type_name()
        )),
    }
}

fn n_type(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let file_meta = lua.file_meta;
    match arg(args, 0) {
        Value::Userdata(u) if lua.userdata[u.0 as usize].metatable == file_meta => {
            let s = if lua.userdata[u.0 as usize].closed {
                "closed file"
            } else {
                "file"
            };
            Ok(vec![lua.new_string(s.as_bytes())])
        }
        _ => Ok(vec![Value::Nil]),
    }
}

fn n_tmpfile(lua: &mut Lua, _args: &[Value]) -> Result<Vec<Value>, String> {
    let file_meta = lua.file_meta.expect("file metatable");
    let name = {
        let Some(h) = lua.host.as_mut() else {
            return Ok(open_failure(lua, HostError::new("io library has no host")));
        };
        h.tmpname()
    };
    let name = match name {
        Ok(n) => n,
        Err(e) => return Ok(open_failure(lua, e)),
    };
    let opened = {
        let Some(h) = lua.host.as_mut() else {
            return Ok(open_failure(lua, HostError::new("io library has no host")));
        };
        h.open(&name, "w+")
    };
    match opened {
        Ok(handle) => Ok(vec![new_file(lua, HostObject::File(handle), file_meta)]),
        Err(e) => Ok(open_failure(lua, e)),
    }
}
