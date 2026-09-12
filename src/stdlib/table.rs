//! The `table` library. `table.sort`, `table.move`, `table.insert` and the
//! metamethod-aware wrappers around `remove`/`concat`/`unpack` live in the
//! Lua prelude (they need to call back into Lua or honor metamethods).
//!
//! The natives installed here for `concat`/`unpack`/`remove` are the raw
//! fast paths the prelude delegates to when the table has no metatable.

use crate::value::{Value, fmt_number};
use crate::vm::Lua;

use super::{arg, check_table, set_field};

pub fn install(lua: &mut Lua) {
    let tt = lua.new_table();
    lua.set_global("table", tt);
    for (name, f) in [
        ("remove", n_remove as crate::vm::NativeFn),
        ("concat", n_concat),
        ("pack", n_pack),
        ("unpack", n_unpack),
    ] {
        let v = lua.add_native(name, f);
        set_field(lua, tt, name, v);
    }
}

fn table_id(lua: &Lua, args: &[Value], i: usize, who: &str) -> Result<u32, String> {
    let Value::Table(id) = check_table(lua, args, i, who)? else {
        unreachable!()
    };
    Ok(id.0)
}

fn opt_int(args: &[Value], i: usize, who: &str) -> Result<Option<i64>, String> {
    match arg(args, i) {
        Value::Nil => Ok(None),
        Value::Int(n) => Ok(Some(n)),
        Value::Float(f) => crate::value::float_to_exact_int(f)
            .map(Some)
            .ok_or_else(|| {
                format!(
                    "bad argument #{} to '{who}' (number has no integer representation)",
                    i + 1
                )
            }),
        v => Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn n_remove(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = table_id(lua, args, 0, "remove")? as usize;
    let size = lua.tables[t].length();
    let mut pos = opt_int(args, 1, "remove")?.unwrap_or(size);
    // PUC: when `pos ~= size`, require (unsigned)pos - 1 <= (unsigned)size.
    if pos != size && (pos as u64).wrapping_sub(1) > size as u64 {
        return Err("bad argument #2 to 'remove' (position out of bounds)".into());
    }
    let removed = lua.tables[t].get(Value::Int(pos));
    while pos < size {
        let v = lua.tables[t].get(Value::Int(pos + 1));
        lua.tables[t]
            .set(Value::Int(pos), v)
            .map_err(|e| e.to_string())?;
        pos += 1;
    }
    lua.tables[t]
        .set(Value::Int(pos), Value::Nil)
        .map_err(|e| e.to_string())?;
    Ok(vec![removed])
}

fn n_concat(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = table_id(lua, args, 0, "concat")? as usize;
    let sep = match arg(args, 1) {
        Value::Nil => Vec::new(),
        Value::Str(id) => lua.strings.get(id).to_vec(),
        v @ (Value::Int(_) | Value::Float(_)) => fmt_number(v).into_bytes(),
        v => {
            return Err(format!(
                "bad argument #2 to 'concat' (string expected, got {})",
                v.type_name()
            ));
        }
    };
    let i = opt_int(args, 2, "concat")?.unwrap_or(1);
    let j = opt_int(args, 3, "concat")?.unwrap_or_else(|| lua.tables[t].length());
    let mut out: Vec<u8> = Vec::new();
    for k in i..=j {
        let v = lua.tables[t].get(Value::Int(k));
        match v {
            Value::Str(id) => out.extend_from_slice(lua.strings.get(id)),
            Value::Int(_) | Value::Float(_) => out.extend_from_slice(fmt_number(v).as_bytes()),
            _ => {
                return Err(format!(
                    "invalid value (at index {k}) in table for 'concat'"
                ));
            }
        }
        if k < j {
            out.extend_from_slice(&sep);
        }
    }
    Ok(vec![lua.new_string(&out)])
}

fn n_pack(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = lua.new_table();
    let Value::Table(id) = t else { unreachable!() };
    for (i, v) in args.iter().enumerate() {
        lua.tables[id.0 as usize]
            .set(Value::Int(i as i64 + 1), *v)
            .map_err(|e| e.to_string())?;
    }
    let n = lua.new_string(b"n");
    lua.tables[id.0 as usize]
        .set(n, Value::Int(args.len() as i64))
        .map_err(|e| e.to_string())?;
    Ok(vec![t])
}

fn n_unpack(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let t = table_id(lua, args, 0, "unpack")? as usize;
    let i = opt_int(args, 1, "unpack")?.unwrap_or(1);
    let j = opt_int(args, 2, "unpack")?.unwrap_or_else(|| lua.tables[t].length());
    if i > j {
        return Ok(vec![]);
    }
    // The range size can exceed `i64` (e.g. `mininteger..maxinteger`), so
    // widen before subtracting; `i128` covers the full span of an i64 range.
    let n = (j as i128) - (i as i128) + 1;
    if n > 1_000_000 {
        return Err("too many results to unpack".into());
    }
    Ok((i..=j).map(|k| lua.tables[t].get(Value::Int(k))).collect())
}
