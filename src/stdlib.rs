//! Base library (M1 subset): pure, allocation-light natives that never call
//! back into Lua. Natives that need to run Lua code (table.sort, pcall as a
//! library function, metamethod-aware tostring) arrive with later milestones
//! as VM intrinsics.

use crate::value::{to_key, Value};
use crate::vm::{CoStatus, Intrinsic, Lua, NativeKind};

pub fn install(lua: &mut Lua) {
    lua.register_native("print", n_print);
    lua.register_native("type", n_type);
    lua.register_native("tonumber", n_tonumber);
    lua.register_native("select", n_select);
    lua.register_native("rawget", n_rawget);
    lua.register_native("rawset", n_rawset);
    lua.register_native("rawequal", n_rawequal);
    lua.register_native("rawlen", n_rawlen);
    lua.register_native("setmetatable", n_setmetatable);
    lua.register_native("getmetatable", n_getmetatable);
    lua.builtin_next = lua.register_native("next", n_next);
    lua.register_native("pairs", n_pairs);
    lua.builtin_ipairs_iter = lua.add_native("(ipairs iterator)", n_ipairs_iter);
    lua.register_native("ipairs", n_ipairs);
    // intrinsics: these interact with frames (raise error values, set up
    // protected calls, call __tostring)
    lua.register_intrinsic("error", Intrinsic::Error);
    lua.register_intrinsic("assert", Intrinsic::Assert);
    lua.register_intrinsic("tostring", Intrinsic::ToString);
    lua.register_intrinsic("pcall", Intrinsic::Pcall);
    lua.register_intrinsic("xpcall", Intrinsic::Xpcall);
    install_coroutine(lua);
}

fn set_field(lua: &mut Lua, t: Value, name: &str, v: Value) {
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
            let Value::Thread(tid) = lua.create_coroutine(f) else { unreachable!() };
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
        _ => {
            return Err("bad argument #2 to 'setmetatable' (nil or table expected)".into())
        }
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

fn arg(args: &[Value], i: usize) -> Value {
    args.get(i).copied().unwrap_or(Value::Nil)
}

fn check_table(_lua: &Lua, args: &[Value], i: usize, who: &str) -> Result<Value, String> {
    match arg(args, i) {
        v @ Value::Table(_) => Ok(v),
        v => Err(format!(
            "bad argument #{} to '{who}' (table expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn n_print(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let line = args
        .iter()
        .map(|v| lua.display_value(*v))
        .collect::<Vec<_>>()
        .join("\t");
    println!("{line}");
    Ok(vec![])
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
/// optional sign and surrounding whitespace).
fn parse_number(bytes: &[u8]) -> Option<Value> {
    use crate::lexer::{Lexer, Token};
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let (negate, text) = match text.strip_prefix('-') {
        Some(rest) => (true, rest.trim_start()),
        None => (false, text.strip_prefix('+').unwrap_or(text).trim_start()),
    };
    let mut lx = Lexer::new(text.as_bytes());
    let (tok, _) = lx.next_token().ok()?;
    let (end, _) = lx.next_token().ok()?;
    if end != Token::Eof {
        return None;
    }
    let v = match tok {
        Token::Int(i) => Value::Int(if negate { i.wrapping_neg() } else { i }),
        Token::Float(f) => Value::Float(if negate { -f } else { f }),
        _ => return None,
    };
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
        _ => Err("table or string expected".into()),
    }
}

fn n_next(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let Value::Table(id) = check_table(lua, args, 0, "next")? else {
        unreachable!()
    };
    let prev = match arg(args, 1) {
        Value::Nil => None,
        k => Some(to_key(k).map_err(|m| m.to_string())?),
    };
    match lua.tables[id.0 as usize].next_after(prev) {
        Some((k, v)) => Ok(vec![k, v]),
        None => Ok(vec![Value::Nil]),
    }
}

fn n_pairs(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = check_table(lua, args, 0, "pairs")?;
    Ok(vec![lua.builtin_next, t, Value::Nil])
}

fn n_ipairs(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = arg(args, 0);
    Ok(vec![lua.builtin_ipairs_iter, t, Value::Int(0)])
}

fn n_ipairs_iter(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = check_table(lua, args, 0, "ipairs")?;
    let i = match arg(args, 1) {
        Value::Int(i) => i,
        _ => return Err("bad argument #2 to 'ipairs' (integer expected)".into()),
    };
    let next = i + 1;
    match lua.table_get(t, Value::Int(next)) {
        Value::Nil => Ok(vec![Value::Nil]),
        v => Ok(vec![Value::Int(next), v]),
    }
}
