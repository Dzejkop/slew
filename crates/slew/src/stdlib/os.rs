//! The `os` library, backed by the embedder-installed [`Host`].
//!
//! `os.execute` and process spawning are deliberately absent: process
//! authority is a non-goal. `os.exit` does **not** terminate the host process;
//! it records a request ([`crate::Lua::take_exit_request`]) and raises a
//! controlled error the embedder can observe.

use crate::host::{DateParts, HostError};
use crate::value::Value;
use crate::vm::Lua;

use super::{arg, check_bytes, set_field};

pub(super) fn install<C>(lua: &mut Lua<C>) {
    let os = lua.new_table();
    lua.set_global("os", os);
    let entries: [(&str, crate::vm::NativeFn<C>); 9] = [
        ("clock", n_clock),
        ("time", n_time),
        ("date", n_date),
        ("difftime", n_difftime),
        ("getenv", n_getenv),
        ("remove", n_remove),
        ("rename", n_rename),
        ("tmpname", n_tmpname),
        ("setlocale", n_setlocale),
    ];
    for (name, f) in entries {
        let v = lua.add_native(name, f);
        set_field(lua, os, name, v);
    }
    let exit = lua.add_native("exit", n_exit);
    set_field(lua, os, "exit", exit);
}

fn err_return<C>(lua: &mut Lua<C>, e: &HostError) -> Vec<Value> {
    let msg = lua.new_string(e.message.as_bytes());
    vec![Value::Nil, msg, Value::Int(e.errno as i64)]
}

fn n_clock<C>(lua: &mut Lua<C>, _args: &[Value]) -> Result<Vec<Value>, String> {
    let t = match lua.host.as_mut() {
        Some(h) => h.clock(),
        None => 0.0,
    };
    Ok(vec![Value::Float(t)])
}

fn table_field<C>(lua: &Lua<C>, t: Value, name: &str) -> Value {
    let Some(key) = lua.strings.lookup(name.as_bytes()) else {
        return Value::Nil;
    };
    lua.table_get(t, Value::Str(key))
}

fn want_int_field<C>(
    lua: &Lua<C>,
    t: Value,
    name: &str,
    default: Option<i64>,
) -> Result<i64, String> {
    match table_field(lua, t, name) {
        Value::Nil => default.ok_or_else(|| format!("field '{name}' missing in date table")),
        Value::Int(i) => Ok(i),
        Value::Float(f) => crate::value::float_to_exact_int(f)
            .ok_or_else(|| format!("field '{name}' is not an integer")),
        v => Err(format!(
            "field '{name}' is not an integer (got {})",
            v.type_name()
        )),
    }
}

fn n_time<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    match arg(args, 0) {
        Value::Nil => {
            let t = match lua.host.as_mut() {
                Some(h) => h.time(),
                None => 0,
            };
            Ok(vec![Value::Int(t)])
        }
        t @ Value::Table(_) => {
            let year = want_int_field(lua, t, "year", None)?;
            let month = want_int_field(lua, t, "month", None)?;
            let day = want_int_field(lua, t, "day", None)?;
            let hour = want_int_field(lua, t, "hour", Some(12))?;
            let min = want_int_field(lua, t, "min", Some(0))?;
            let sec = want_int_field(lua, t, "sec", Some(0))?;
            if !(1..=12).contains(&month) {
                return Err("field 'month' is out-of-bound".into());
            }
            if !(1..=31).contains(&day) {
                return Err("field 'day' is out-of-bound".into());
            }
            if !(0..=23).contains(&hour) {
                return Err("field 'hour' is out-of-bound".into());
            }
            if !(0..=59).contains(&min) {
                return Err("field 'min' is out-of-bound".into());
            }
            if !(0..=61).contains(&sec) {
                return Err("field 'sec' is out-of-bound".into());
            }
            let isdst = matches!(table_field(lua, t, "isdst"), Value::Bool(true));
            let parts = DateParts {
                year,
                month,
                day,
                hour,
                min,
                sec,
                wday: 0,
                yday: 0,
                isdst,
            };
            let v = match lua.host.as_mut() {
                Some(h) => h.make_time(parts),
                None => 0,
            };
            Ok(vec![Value::Int(v)])
        }
        v => Err(format!(
            "bad argument #1 to 'time' (table expected, got {})",
            v.type_name()
        )),
    }
}

const WDAY_ABBR: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const WDAY_FULL: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const MON_ABBR: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const MON_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];

fn wday_index(p: &DateParts) -> usize {
    p.wday.rem_euclid(7) as usize
}

/// Formats into `out` without requiring UTF-8. The conversion specifiers only
/// ever emit ASCII, but literal bytes from the format string must pass through
/// unchanged (Lua strings are byte strings).
fn push_fmt(out: &mut Vec<u8>, args: std::fmt::Arguments<'_>) {
    let s = std::fmt::format(args);
    out.extend_from_slice(s.as_bytes());
}

fn strftime(p: &DateParts, fmt: &[u8]) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i]);
            i += 1;
            continue;
        }
        i += 1;
        let Some(&c) = fmt.get(i) else {
            return Err("invalid conversion specifier '%'".into());
        };
        i += 1;
        let wday = wday_index(p);
        let yday0 = p.yday - 1;
        match c {
            b'a' => out.extend_from_slice(WDAY_ABBR[wday].as_bytes()),
            b'A' => out.extend_from_slice(WDAY_FULL[wday].as_bytes()),
            b'b' | b'h' => {
                out.extend_from_slice(MON_ABBR[(p.month - 1).clamp(0, 11) as usize].as_bytes());
            }
            b'B' => {
                out.extend_from_slice(MON_FULL[(p.month - 1).clamp(0, 11) as usize].as_bytes());
            }
            b'c' => push_fmt(
                &mut out,
                format_args!(
                    "{} {} {:2} {:02}:{:02}:{:02} {}",
                    WDAY_ABBR[wday],
                    MON_ABBR[(p.month - 1).clamp(0, 11) as usize],
                    p.day,
                    p.hour,
                    p.min,
                    p.sec,
                    p.year
                ),
            ),
            b'd' => push_fmt(&mut out, format_args!("{:02}", p.day)),
            b'e' => push_fmt(&mut out, format_args!("{:2}", p.day)),
            b'H' => push_fmt(&mut out, format_args!("{:02}", p.hour)),
            b'I' => {
                let h = p.hour % 12;
                push_fmt(&mut out, format_args!("{:02}", if h == 0 { 12 } else { h }));
            }
            b'j' => push_fmt(&mut out, format_args!("{:03}", p.yday)),
            b'm' => push_fmt(&mut out, format_args!("{:02}", p.month)),
            b'M' => push_fmt(&mut out, format_args!("{:02}", p.min)),
            b'p' => out.extend_from_slice(if p.hour < 12 { b"AM" } else { b"PM" }),
            b'S' => push_fmt(&mut out, format_args!("{:02}", p.sec)),
            b'U' => push_fmt(
                &mut out,
                format_args!("{:02}", (yday0 + 7 - p.wday.rem_euclid(7)) / 7),
            ),
            b'w' => push_fmt(&mut out, format_args!("{}", p.wday.rem_euclid(7))),
            b'W' => {
                let mon = (p.wday.rem_euclid(7) + 6) % 7;
                push_fmt(&mut out, format_args!("{:02}", (yday0 + 7 - mon) / 7));
            }
            b'x' => push_fmt(
                &mut out,
                format_args!("{:02}/{:02}/{:02}", p.month, p.day, p.year.rem_euclid(100)),
            ),
            b'X' => push_fmt(
                &mut out,
                format_args!("{:02}:{:02}:{:02}", p.hour, p.min, p.sec),
            ),
            b'y' => push_fmt(&mut out, format_args!("{:02}", p.year.rem_euclid(100))),
            b'Y' => push_fmt(&mut out, format_args!("{}", p.year)),
            b'Z' => out.extend_from_slice(b"UTC"),
            b'%' => out.push(b'%'),
            _ => return Err(format!("invalid conversion specifier '%{}'", c as char)),
        }
    }
    Ok(out)
}

fn n_date<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let fmt = match arg(args, 0) {
        Value::Nil => b"%c".to_vec(),
        v => check_bytes(lua, &[v], 0, "date")?,
    };
    let t = match arg(args, 1) {
        Value::Nil => match lua.host.as_mut() {
            Some(h) => h.time(),
            None => 0,
        },
        Value::Int(i) => i,
        Value::Float(f) => f as i64,
        v => {
            return Err(format!(
                "bad argument #2 to 'date' (number expected, got {})",
                v.type_name()
            ));
        }
    };
    let (utc, fmt) = match fmt.strip_prefix(b"!") {
        Some(rest) => (true, rest.to_vec()),
        None => (false, fmt),
    };
    let parts = match lua.host.as_mut() {
        Some(h) => h.time_parts(t, utc),
        None => DateParts {
            year: 1970,
            month: 1,
            day: 1,
            hour: 0,
            min: 0,
            sec: 0,
            wday: 4,
            yday: 1,
            isdst: false,
        },
    };
    if fmt == b"*t" {
        let table = lua.new_table();
        let fields: [(&str, i64); 8] = [
            ("year", parts.year),
            ("month", parts.month),
            ("day", parts.day),
            ("hour", parts.hour),
            ("min", parts.min),
            ("sec", parts.sec),
            ("wday", parts.wday.rem_euclid(7) + 1),
            ("yday", parts.yday),
        ];
        for (name, v) in fields {
            set_field(lua, table, name, Value::Int(v));
        }
        set_field(lua, table, "isdst", Value::Bool(parts.isdst));
        return Ok(vec![table]);
    }
    let s = strftime(&parts, &fmt)?;
    Ok(vec![lua.new_string(&s)])
}

fn n_difftime<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    // PUC's `os_difftime` checks BOTH arguments with `l_checktime`, i.e.
    // `luaL_checkinteger`: integral floats and numeric strings coerce, while a
    // missing, nil, or fractional argument is an error.
    let t1 = check_time(lua, arg(args, 0), 1)?;
    let t2 = check_time(lua, arg(args, 1), 2)?;
    // Compute in f64 so a difference wider than i64 does not wrap.
    Ok(vec![Value::Float(t1 as f64 - t2 as f64)])
}

/// `luaL_checkinteger`-ish for `os.difftime`: integers pass, integral floats
/// and numeric strings coerce, everything else is a type/representation error.
fn check_time<C>(lua: &Lua<C>, v: Value, argno: usize) -> Result<i64, String> {
    let n = match v {
        Value::Int(i) => return Ok(i),
        Value::Float(f) => f,
        Value::Str(s) => match super::parse_number(lua.strings.get(s)) {
            Some(Value::Int(i)) => return Ok(i),
            Some(Value::Float(f)) => f,
            _ => {
                return Err(format!(
                    "bad argument #{argno} to 'difftime' (number expected, got string)"
                ));
            }
        },
        other => {
            return Err(format!(
                "bad argument #{argno} to 'difftime' (number expected, got {})",
                other.type_name()
            ));
        }
    };
    crate::value::float_to_exact_int(n).ok_or_else(|| {
        format!("bad argument #{argno} to 'difftime' (number has no integer representation)")
    })
}

fn n_getenv<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let name = check_bytes(lua, args, 0, "getenv")?;
    let name = String::from_utf8_lossy(&name).into_owned();
    let val = match lua.host.as_mut() {
        Some(h) => h.getenv(&name),
        None => None,
    };
    match val {
        Some(bytes) => Ok(vec![lua.new_string(&bytes)]),
        None => Ok(vec![Value::Nil]),
    }
}

fn fs_call<C>(
    lua: &mut Lua<C>,
    args: &[Value],
    who: &str,
    f: impl FnOnce(&mut dyn crate::host::Host, String, String) -> Result<(), HostError>,
) -> Result<Vec<Value>, String> {
    let from = String::from_utf8_lossy(&check_bytes(lua, args, 0, who)?).into_owned();
    let to = if who == "rename" {
        String::from_utf8_lossy(&check_bytes(lua, args, 1, who)?).into_owned()
    } else {
        String::new()
    };
    let result = match lua.host.as_mut() {
        Some(h) => f(h.as_mut(), from, to),
        None => Err(HostError::new("os library has no host")),
    };
    match result {
        Ok(()) => Ok(vec![Value::Bool(true)]),
        Err(e) => Ok(err_return(lua, &e)),
    }
}

fn n_remove<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    fs_call(lua, args, "remove", |h, from, _| h.remove(&from))
}

fn n_rename<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    fs_call(lua, args, "rename", |h, from, to| h.rename(&from, &to))
}

fn n_tmpname<C>(lua: &mut Lua<C>, _args: &[Value]) -> Result<Vec<Value>, String> {
    let name = match lua.host.as_mut() {
        Some(h) => h.tmpname(),
        None => Err(HostError::new("os library has no host")),
    };
    match name {
        Ok(n) => Ok(vec![lua.new_string(n.as_bytes())]),
        Err(e) => Ok(err_return(lua, &e)),
    }
}

fn n_setlocale<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let locale = match arg(args, 0) {
        Value::Nil => None,
        v => Some(String::from_utf8_lossy(&check_bytes(lua, &[v], 0, "setlocale")?).into_owned()),
    };
    let category = match arg(args, 1) {
        Value::Nil => None,
        v => Some(String::from_utf8_lossy(&check_bytes(lua, &[v], 0, "setlocale")?).into_owned()),
    };
    if let Some(c) = &category {
        const CATEGORIES: [&str; 6] = ["all", "collate", "ctype", "monetary", "numeric", "time"];
        if !CATEGORIES.contains(&c.as_str()) {
            return Err(format!(
                "bad argument #2 to 'setlocale' (invalid option '{c}')"
            ));
        }
    }
    let result = match lua.host.as_mut() {
        Some(h) => h.setlocale(locale.as_deref(), category.as_deref()),
        None => None,
    };
    match result {
        Some(s) => Ok(vec![lua.new_string(s.as_bytes())]),
        None => Ok(vec![Value::Nil]),
    }
}

fn n_exit<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let code = match arg(args, 0) {
        Value::Nil | Value::Bool(true) => 0,
        Value::Bool(false) => 1,
        Value::Int(i) => i,
        Value::Float(f) => f as i64,
        v => {
            return Err(format!(
                "bad argument #1 to 'exit' (number expected, got {})",
                v.type_name()
            ));
        }
    };
    let close = arg(args, 1).truthy();
    if let Some(h) = lua.host.as_mut() {
        h.request_exit(code, close);
    }
    lua.exit_request = Some(code);
    Err(format!("os.exit({code})"))
}
