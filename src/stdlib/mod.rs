//! The standard library: base functions, `coroutine`, `string` (with the
//! pattern engine), `table`, `math`, and `utf8`.
//!
//! Functions that call back into Lua code (table.sort's comparator,
//! gsub's function replacements, gmatch iterators) are defined in
//! `prelude.lua`, compiled and run at `Lua::new()` — that way they execute
//! through the regular suspendable VM machinery instead of needing native
//! reentrancy. `os` and `io` are deliberately absent: they are
//! nondeterministic ambient authority; embedders can register their own.

mod dump;
mod io;
mod math;
mod os;
pub(crate) mod string;
mod string_pack;
mod table;
mod utf8;

use crate::value::{Value, fmt_number, to_key};
use crate::vm::{CoStatus, Error, Intrinsic, Lua, NativeKind, Step};

pub fn install(lua: &mut Lua) {
    lua.register_intrinsic("print", Intrinsic::Print);
    lua.register_native("type", n_type);
    lua.register_native("tonumber", n_tonumber);
    lua.register_native("select", n_select);
    lua.register_native("rawget", n_rawget);
    lua.register_native("rawset", n_rawset);
    lua.register_native("rawequal", n_rawequal);
    lua.register_native("rawlen", n_rawlen);
    lua.register_intrinsic("collectgarbage", Intrinsic::CollectGarbage);
    lua.register_native("setmetatable", n_setmetatable);
    lua.register_native("getmetatable", n_getmetatable);
    // Private helper for the prelude's `__pairs` lookup: the public
    // `getmetatable` honors the `__metatable` guard, but metamethod lookup
    // must bypass it (PUC's `luaL_getmetafield`). The prelude captures this
    // and clears the global immediately.
    lua.register_native("__slew_getmetatable", n_raw_metatable);
    lua.register_native("next", n_next);
    // `pairs`/`ipairs` and `print` are defined in the prelude (they must call
    // metamethods, which natives cannot do).
    // intrinsics: these interact with frames (raise error values, set up
    // protected calls, call __tostring)
    lua.register_intrinsic("error", Intrinsic::Error);
    lua.register_intrinsic("assert", Intrinsic::Assert);
    lua.register_intrinsic("tostring", Intrinsic::ToString);
    lua.register_intrinsic("pcall", Intrinsic::Pcall);
    lua.register_intrinsic("xpcall", Intrinsic::Xpcall);
    install_coroutine(lua);
    string::install(lua);
    table::install(lua);
    math::install(lua);
    utf8::install(lua);
    install_debug(lua);
    // dynamic loading: `load` is pure; `loadfile` goes through the host
    // reader installed with `Lua::set_file_reader` (none by default)
    lua.register_native("load", n_load);
    lua.register_native("loadfile", n_loadfile);
    install_package(lua);
    run_prelude(lua);
}

/// Registers the `io`/`os` libraries once an embedder has installed a host.
/// Building the file metatable first lets userdata reference it; the small Lua
/// prelude then adds the `lines` iterators. This is never called without a
/// host, so `io`/`os` are simply absent and the core keeps no authority.
pub(crate) fn install_host_libs(lua: &mut Lua) {
    let file_meta = io::build_file_metatable(lua);
    lua.file_meta = Some(file_meta);
    io::install(lua);
    os::install(lua);

    let io_v = lua.get_global("io");
    let os_v = lua.get_global("os");
    let loaded_key = lua.new_string(b"loaded");
    let pkg = lua.get_global("package");
    let loaded = lua.table_get(pkg, loaded_key);
    if let Value::Table(id) = loaded {
        set_field(lua, Value::Table(id), "io", io_v);
        set_field(lua, Value::Table(id), "os", os_v);
    }
    io::run_prelude(lua);
}

/// The introspective `debug` library. These are intrinsics (not plain
/// natives) because they inspect call frames, and the running thread is
/// `mem::take`n out of the arena during dispatch: only intrinsics receive
/// the live `&mut Thread`.
fn install_debug(lua: &mut Lua) {
    let debug = lua.new_table();
    lua.set_global("debug", debug);
    let entries: [(&str, Intrinsic); 10] = [
        ("getinfo", Intrinsic::DebugGetinfo),
        ("traceback", Intrinsic::DebugTraceback),
        ("getupvalue", Intrinsic::DebugGetupvalue),
        ("setupvalue", Intrinsic::DebugSetupvalue),
        ("upvalueid", Intrinsic::DebugUpvalueid),
        ("upvaluejoin", Intrinsic::DebugUpvaluejoin),
        ("getmetatable", Intrinsic::DebugGetmetatable),
        ("setmetatable", Intrinsic::DebugSetmetatable),
        ("getregistry", Intrinsic::DebugGetregistry),
        // `gethook`/`sethook` are not implemented (tier c); `gethook`
        // returns nil so "no hook" probes succeed.
        ("gethook", Intrinsic::DebugGethook),
    ];
    for (name, i) in entries {
        let f = lua.add_native_kind(name, NativeKind::Intrinsic(i));
        set_field(lua, debug, name, f);
    }
}

fn run_prelude(lua: &mut Lua) {
    let chunk = lua
        .load_named("prelude", include_str!("prelude.lua"))
        .expect("prelude must compile");
    let mut exec = lua.execute(&chunk);
    loop {
        match exec.step(lua, 1_000_000) {
            Ok(Step::Done(_)) => break,
            Ok(Step::Pending) => continue,
            Err(e) => panic!("prelude failed: {e}"),
        }
    }
}

pub(super) fn set_field(lua: &mut Lua, t: Value, name: &str, v: Value) {
    let Value::Table(id) = t else { unreachable!() };
    let k = lua.new_string(name.as_bytes());
    lua.tables[id.0 as usize].set(k, v).unwrap();
}

fn install_coroutine(lua: &mut Lua) {
    let ct = lua.new_table();
    lua.set_global("coroutine", ct);
    let create = lua.add_native("create", n_co_create);
    set_field(lua, ct, "create", create);
    let status = lua.add_native("status", n_co_status);
    set_field(lua, ct, "status", status);
    let wrap = lua.add_native("wrap", n_co_wrap);
    set_field(lua, ct, "wrap", wrap);
    let resume = lua.add_native_kind("resume", NativeKind::Intrinsic(Intrinsic::Resume));
    set_field(lua, ct, "resume", resume);
    let yield_ = lua.add_native_kind("yield", NativeKind::Intrinsic(Intrinsic::Yield));
    set_field(lua, ct, "yield", yield_);
    let isyieldable =
        lua.add_native_kind("isyieldable", NativeKind::Intrinsic(Intrinsic::IsYieldable));
    set_field(lua, ct, "isyieldable", isyieldable);
    let running = lua.add_native_kind("running", NativeKind::Intrinsic(Intrinsic::Running));
    set_field(lua, ct, "running", running);
    let close = lua.add_native_kind("close", NativeKind::Intrinsic(Intrinsic::CoroutineClose));
    set_field(lua, ct, "close", close);
}

/// Creates the `package` table: `loaded`/`preload`/paths plus the
/// `searchpath` probe. `require` and the searchers live in the prelude so
/// they can call back into Lua loaders through the regular VM machinery.
fn install_package(lua: &mut Lua) {
    let pkg = lua.new_table();
    lua.set_global("package", pkg);
    let loaded = lua.new_table();
    set_field(lua, pkg, "loaded", loaded);
    let preload = lua.new_table();
    set_field(lua, pkg, "preload", preload);
    let path = lua.new_string(b"?.lua");
    set_field(lua, pkg, "path", path);
    let cpath = lua.new_string(b"");
    set_field(lua, pkg, "cpath", cpath);
    let config = lua.new_string(b"/\n;\n?\n!\n-\n");
    set_field(lua, pkg, "config", config);
    let searchpath = lua.add_native("searchpath", n_searchpath);
    set_field(lua, pkg, "searchpath", searchpath);

    let globals = Value::Table(lua.globals);
    lua.set_global("_G", globals);
    set_field(lua, loaded, "_G", globals);
    for name in [
        "string",
        "table",
        "math",
        "utf8",
        "coroutine",
        "package",
        "debug",
    ] {
        let v = lua.get_global(name);
        set_field(lua, loaded, name, v);
    }
}

/// Compiles `src` the way `load`/`loadfile` do: the function on success,
/// `nil, message` on any failure. Text/binary `mode` is enforced, and binary
/// chunks are decoded by [`dump::undump`].
fn load_source(
    lua: &mut Lua,
    src: Vec<u8>,
    chunkname: &str,
    mode: &[u8],
    env: Option<Value>,
) -> Vec<Value> {
    if dump::is_binary(&src) {
        if !mode.contains(&b'b') {
            return vec![
                Value::Nil,
                lua.new_string(b"attempt to load a binary chunk (mode is 't')"),
            ];
        }
        return match dump::undump(lua, &src, env) {
            Ok(f) => vec![f],
            Err(msg) => {
                let full = format!("{chunkname}: {msg}");
                vec![Value::Nil, lua.new_string(full.as_bytes())]
            }
        };
    }
    if !mode.contains(&b't') {
        return vec![
            Value::Nil,
            lua.new_string(b"attempt to load a text chunk (mode is 'b')"),
        ];
    }
    match lua.load_named(chunkname, &src) {
        Ok(chunk) => vec![lua.make_function(&chunk, env)],
        Err(Error::Parse(e)) => text_error(lua, chunkname, e.line, &e.message),
        Err(Error::Compile(e)) => text_error(lua, chunkname, e.line, &e.message),
        Err(Error::Runtime(e)) => {
            vec![Value::Nil, lua.new_string(e.message.as_bytes())]
        }
    }
}

/// `nil, "chunkname:line: message"`, the shape `load`/`loadfile` use for
/// compile failures.
fn text_error(lua: &mut Lua, chunkname: &str, line: u32, message: &str) -> Vec<Value> {
    let msg = format!("{chunkname}:{line}: {message}");
    vec![Value::Nil, lua.new_string(msg.as_bytes())]
}

/// `luaL_checkstring`-ish: strings pass, numbers are rendered, anything
/// else (including nil) is an error.
pub(super) fn check_bytes(
    lua: &Lua,
    args: &[Value],
    i: usize,
    who: &str,
) -> Result<Vec<u8>, String> {
    match arg(args, i) {
        Value::Str(id) => Ok(lua.strings.get(id).to_vec()),
        v @ (Value::Int(_) | Value::Float(_)) => Ok(fmt_number(v).into_bytes()),
        v => Err(format!(
            "bad argument #{} to '{who}' (string expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn opt_bytes(lua: &Lua, args: &[Value], i: usize, who: &str) -> Result<Option<Vec<u8>>, String> {
    if arg(args, i) == Value::Nil {
        Ok(None)
    } else {
        check_bytes(lua, args, i, who).map(Some)
    }
}

fn replace_all(hay: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    if needle.is_empty() {
        return hay.to_vec();
    }
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

fn n_load(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let src = check_bytes(lua, args, 0, "load")?;
    // PUC defaults the chunk name of a string chunk to the source itself
    let chunkname = match opt_bytes(lua, args, 1, "load")? {
        Some(name) => String::from_utf8_lossy(&name).into_owned(),
        None => String::from_utf8_lossy(&src).into_owned(),
    };
    let mode = opt_bytes(lua, args, 2, "load")?.unwrap_or_else(|| b"bt".to_vec());
    // an explicitly passed `env` (even nil) replaces `_ENV`; absent means
    // the globals table
    let env = (args.len() > 3).then(|| arg(args, 3));
    Ok(load_source(lua, src, &chunkname, &mode, env))
}

fn n_loadfile(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let Some(filename) = opt_bytes(lua, args, 0, "loadfile")? else {
        return Ok(vec![
            Value::Nil,
            lua.new_string(b"cannot read stdin: no input stream"),
        ]);
    };
    let name = String::from_utf8_lossy(&filename).into_owned();
    let mode = opt_bytes(lua, args, 1, "loadfile")?.unwrap_or_else(|| b"bt".to_vec());
    let env = (args.len() > 2).then(|| arg(args, 2));
    match lua.read_file(&name) {
        Ok(Some(src)) => Ok(load_source(lua, src, &name, &mode, env)),
        Ok(None) => {
            let msg = format!("cannot open {name}");
            Ok(vec![Value::Nil, lua.new_string(msg.as_bytes())])
        }
        Err(e) => {
            let msg = format!("cannot open {name}: {e}");
            Ok(vec![Value::Nil, lua.new_string(msg.as_bytes())])
        }
    }
}

/// `package.searchpath(name, path [, sep [, rep]])`: substitutes `sep` with
/// `rep` in `name`, replaces every `?` in each `;`-separated template, and
/// probes candidates through the host reader. On failure returns
/// `nil, "no file '...'\n\tno file '...'"` like PUC (built from the whole
/// expanded path, so empty templates still produce their line). A hard
/// reader error counts as "not readable", matching PUC's `fopen` probe.
fn n_searchpath(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let name = check_bytes(lua, args, 0, "searchpath")?;
    let path = check_bytes(lua, args, 1, "searchpath")?;
    let sep = opt_bytes(lua, args, 2, "searchpath")?.unwrap_or_else(|| b".".to_vec());
    let rep = opt_bytes(lua, args, 3, "searchpath")?.unwrap_or_else(|| b"/".to_vec());
    let name = if sep.is_empty() {
        name
    } else {
        replace_all(&name, &sep, &rep)
    };
    let expanded = replace_all(&path, b"?", &name);
    for tmpl in expanded.split(|&b| b == b';') {
        if tmpl.is_empty() {
            continue;
        }
        let candidate = String::from_utf8_lossy(tmpl);
        if matches!(lua.read_file(&candidate), Ok(Some(_))) {
            return Ok(vec![lua.new_string(tmpl)]);
        }
    }
    let mut msg = Vec::from(&b"no file '"[..]);
    msg.extend_from_slice(&replace_all(&expanded, b";", b"'\n\tno file '"));
    msg.push(b'\'');
    Ok(vec![Value::Nil, lua.new_string(&msg)])
}

fn n_co_create(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match arg(args, 0) {
        f @ (Value::Closure(_) | Value::Native(_)) => Ok(vec![lua.create_coroutine(f)]),
        v => Err(format!(
            "bad argument #1 to 'create' (function expected, got {})",
            v.type_name()
        )),
    }
}

fn n_co_status(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let Value::Thread(co) = arg(args, 0) else {
        return Err("bad argument #1 to 'status' (coroutine expected)".into());
    };
    let s: &str = if co == lua.current_thread {
        "running"
    } else {
        match lua.threads[co.0 as usize].status {
            CoStatus::Start | CoStatus::Suspended => "suspended",
            CoStatus::Normal => "normal",
            CoStatus::Running => "running",
            CoStatus::Dead => "dead",
        }
    };
    Ok(vec![lua.new_string(s.as_bytes())])
}

fn n_co_wrap(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match arg(args, 0) {
        f @ (Value::Closure(_) | Value::Native(_)) => {
            let Value::Thread(tid) = lua.create_coroutine(f) else {
                unreachable!()
            };
            let wrapper = lua.add_native_kind(
                "(coroutine wrapper)",
                NativeKind::Intrinsic(Intrinsic::WrapResume(tid)),
            );
            Ok(vec![wrapper])
        }
        v => Err(format!(
            "bad argument #1 to 'wrap' (function expected, got {})",
            v.type_name()
        )),
    }
}

fn n_setmetatable(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = check_table(lua, args, 0, "setmetatable")?;
    let Value::Table(id) = t else { unreachable!() };
    let mt = match arg(args, 1) {
        Value::Nil => None,
        Value::Table(m) => Some(m),
        _ => return Err("bad argument #2 to 'setmetatable' (nil or table expected)".into()),
    };
    if lua.metamethod_pub(t, "__metatable") != Value::Nil {
        return Err("cannot change a protected metatable".into());
    }
    lua.tables[id.0 as usize].metatable = mt;
    Ok(vec![t])
}

fn n_getmetatable(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let v = arg(args, 0);
    match lua.get_metatable(v) {
        None => Ok(vec![Value::Nil]),
        Some(mt) => {
            let protected = lua.metamethod_pub(v, "__metatable");
            if protected != Value::Nil {
                Ok(vec![protected])
            } else {
                Ok(vec![Value::Table(mt)])
            }
        }
    }
}

/// The raw metatable (ignoring a `__metatable` guard), for the prelude's
/// metamethod lookups.
fn n_raw_metatable(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match lua.get_metatable(arg(args, 0)) {
        Some(mt) => Ok(vec![Value::Table(mt)]),
        None => Ok(vec![Value::Nil]),
    }
}

pub(super) fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or(Value::Nil)
}

pub(super) fn check_table(
    _lua: &Lua,
    args: &[Value],
    i: usize,
    who: &str,
) -> Result<Value, String> {
    match arg(args, i) {
        v @ Value::Table(_) => Ok(v),
        v => Err(format!(
            "bad argument #{} to '{who}' (table expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn n_type(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    if args.is_empty() {
        return Err("bad argument #1 to 'type' (value expected)".into());
    }
    Ok(vec![lua.new_string(args[0].type_name().as_bytes())])
}

fn n_tonumber(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match arg(args, 1) {
        Value::Nil => Ok(vec![match arg(args, 0) {
            v @ (Value::Int(_) | Value::Float(_)) => v,
            Value::Str(id) => {
                let bytes = lua.strings.get(id).to_vec();
                parse_number(&bytes).unwrap_or(Value::Nil)
            }
            _ => Value::Nil,
        }]),
        base_v => {
            let base = match base_v {
                Value::Int(b) if (2..=36).contains(&b) => b,
                _ => return Err("bad argument #2 to 'tonumber' (base out of range)".into()),
            };
            let Value::Str(id) = arg(args, 0) else {
                return Err("bad argument #1 to 'tonumber' (string expected)".into());
            };
            let s = lua.strings.get(id);
            let s = std::str::from_utf8(s).map_err(|_| "invalid string".to_string());
            Ok(vec![match s {
                Ok(s) => parse_int_base(s.trim(), base).map_or(Value::Nil, Value::Int),
                Err(_) => Value::Nil,
            }])
        }
    }
}

/// Parses a Lua numeral (as the `tonumber` builtin: full literal syntax,
/// optional sign and surrounding whitespace). Also used by the VM for
/// arithmetic string coercion.
pub(crate) fn parse_number(bytes: &[u8]) -> Option<Value> {
    use crate::lexer::{Lexer, Token};
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let (negate, body) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text.strip_prefix('+').unwrap_or(text)),
    };
    // PUC's `strtod`/integer scanners allow no whitespace between a sign and
    // the digits (`tonumber("+ 0.01")` is nil).
    if body.starts_with(char::is_whitespace) {
        return None;
    }
    let mut lx = Lexer::new(body.as_bytes());
    let (tok, _) = lx.next_token().ok()?;
    let (end, _) = lx.next_token().ok()?;
    if end != Token::Eof {
        return None;
    }
    let v = match tok {
        Token::Int(i) => Value::Int(if negate { i.wrapping_neg() } else { i }),
        Token::Float(f) => {
            // A decimal integer that overflows `i64` lexes as a float. PUC,
            // however, parses the sign together with the digits and lets the
            // magnitude reach 2^63 before wrapping, so
            // `tonumber("-9223372036854775808")` is LUA_MININTEGER, not a
            // float. Only the signed minimum is rescued this way; anything
            // larger is a genuine float numeral.
            match (negate, decimal_digits(body)) {
                (true, Some(m)) if m <= 1u128 << 63 => Value::Int((m as u64).wrapping_neg() as i64),
                _ => Value::Float(if negate { -f } else { f }),
            }
        }
        _ => return None,
    };
    Some(v)
}

/// If `s` is a non-empty run of ASCII digits, returns its value (saturating on
/// overflow), used to detect decimal integers just past `i64::MAX`.
fn decimal_digits(s: &str) -> Option<u128> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut v: u128 = 0;
    for b in s.bytes() {
        v = v.saturating_mul(10).saturating_add((b - b'0') as u128);
    }
    Some(v)
}

fn parse_int_base(s: &str, base: i64) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    let (negate, digits) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    if digits.is_empty() {
        return None;
    }
    let mut v: i64 = 0;
    for c in digits.chars() {
        let d = c.to_digit(36)? as i64;
        if d >= base {
            return None;
        }
        v = v.wrapping_mul(base).wrapping_add(d);
    }
    Some(if negate { v.wrapping_neg() } else { v })
}

fn n_select(_lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let rest = args.get(1..).unwrap_or(&[]);
    match arg(args, 0) {
        Value::Str(_) => Ok(vec![Value::Int(rest.len() as i64)]), // select('#', ...)
        Value::Int(i) if i > 0 => {
            let start = (i as usize - 1).min(rest.len());
            Ok(rest[start..].to_vec())
        }
        Value::Int(i) if i < 0 => {
            let start = rest.len().saturating_sub(i.unsigned_abs() as usize);
            Ok(rest[start..].to_vec())
        }
        _ => Err("bad argument #1 to 'select' (index out of range)".into()),
    }
}

fn n_rawget(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = check_table(lua, args, 0, "rawget")?;
    Ok(vec![lua.table_get(t, arg(args, 1))])
}

fn n_rawset(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = check_table(lua, args, 0, "rawset")?;
    let Value::Table(id) = t else { unreachable!() };
    lua.tables[id.0 as usize]
        .set(arg(args, 1), arg(args, 2))
        .map_err(|m| m.to_string())?;
    Ok(vec![t])
}

fn n_rawequal(_lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![Value::Bool(crate::vm::values_equal(
        arg(args, 0),
        arg(args, 1),
    ))])
}

fn n_rawlen(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match arg(args, 0) {
        Value::Table(t) => Ok(vec![Value::Int(lua.tables[t.0 as usize].length())]),
        Value::Str(s) => Ok(vec![Value::Int(lua.strings.get(s).len() as i64)]),
        // PUC's `luaL_argerror` frames the failure with the argument context,
        // e.g. "bad argument #1 to 'rawlen' (table or string expected, got FILE*)".
        v => Err(format!(
            "bad argument #1 to 'rawlen' (table or string expected, got {})",
            v.type_name()
        )),
    }
}

fn n_next(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let Value::Table(id) = check_table(lua, args, 0, "next")? else {
        unreachable!()
    };
    let prev = match arg(args, 1) {
        Value::Nil => None,
        // only nil and NaN can fail: both mean the key can never be in a table
        k => Some(to_key(k).map_err(|_| "invalid key to 'next'".to_string())?),
    };
    match lua.tables[id.0 as usize].next_after(prev) {
        Ok(Some((k, v))) => Ok(vec![k, v]),
        Ok(None) => Ok(vec![Value::Nil]),
        Err(_) => Err("invalid key to 'next'".into()),
    }
}
