//! The `os` library, backed by the embedder-installed [`Host`].
//!
//! `os.execute` and process spawning are deliberately absent: process
//! authority is a non-goal. `os.exit` does **not** terminate the host process;
//! it records a request ([`crate::Lua::take_exit_request`]) and raises a
//! controlled error the embedder can observe.

use std::fmt::Write as _;

use crate::host::{DateParts, HostError};
use crate::value::Value;
use crate::vm::Lua;

use super::{arg, check_bytes, set_field};

pub(super) fn install(lua: &mut Lua) {
    let os = lua.new_table();
    lua.set_global("os", os);
    let entries: [(&str, crate::vm::NativeFn); 9] = [
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

fn err_return(lua: &mut Lua, e: &HostError) -> Vec<Value> {
    let msg = lua.new_string(e.message.as_bytes());
    vec![Value::Nil, msg, Value::Int(e.errno as i64)]
}

fn n_clock(lua: &mut Lua, _args: &[Value]) -> Result<Vec<Value>, String> {
    let t = match lua.host.as_mut() {
        Some(h) => h.clock(),
        None => 0.0,
    };
    Ok(vec![Value::Float(t)])
}

fn table_field(lua: &Lua, t: Value, name: &str) -> Value {
    let Some(key) = lua.strings.lookup(name.as_bytes()) else {
        return Value::Nil;
    };
    lua.table_get(t, Value::Str(key))
}

fn want_int_field(lua: &Lua, t: Value, name: &str, default: Option<i64>) -> Result<i64, String> {
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

fn n_time(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
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

fn strftime(p: &DateParts, fmt: &[u8]) -> Result<String, String> {
    let mut out = String::new();
    let mut i = 0;
    while i < fmt.len() {
        if fmt[i] != b'%' {
            out.push(fmt[i] as char);
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
            b'a' => out.push_str(WDAY_ABBR[wday]),
            b'A' => out.push_str(WDAY_FULL[wday]),
            b'b' | b'h' => out.push_str(MON_ABBR[(p.month - 1).clamp(0, 11) as usize]),
            b'B' => out.push_str(MON_FULL[(p.month - 1).clamp(0, 11) as usize]),
            b'c' => {
                let _ = write!(
                    out,
                    "{} {} {:2} {:02}:{:02}:{:02} {}",
                    WDAY_ABBR[wday],
                    MON_ABBR[(p.month - 1).clamp(0, 11) as usize],
                    p.day,
                    p.hour,
                    p.min,
                    p.sec,
                    p.year
                );
            }
            b'd' => {
                let _ = write!(out, "{:02}", p.day);
            }
            b'e' => {
                let _ = write!(out, "{:2}", p.day);
            }
            b'H' => {
                let _ = write!(out, "{:02}", p.hour);
            }
            b'I' => {
                let h = p.hour % 12;
                let _ = write!(out, "{:02}", if h == 0 { 12 } else { h });
            }
            b'j' => {
                let _ = write!(out, "{:03}", p.yday);
            }
            b'm' => {
                let _ = write!(out, "{:02}", p.month);
            }
            b'M' => {
                let _ = write!(out, "{:02}", p.min);
            }
            b'p' => out.push_str(if p.hour < 12 { "AM" } else { "PM" }),
            b'S' => {
                let _ = write!(out, "{:02}", p.sec);
            }
            b'U' => {
                let _ = write!(out, "{:02}", (yday0 + 7 - p.wday.rem_euclid(7)) / 7);
            }
            b'w' => {
                let _ = write!(out, "{}", p.wday.rem_euclid(7));
            }
            b'W' => {
                let mon = (p.wday.rem_euclid(7) + 6) % 7;
                let _ = write!(out, "{:02}", (yday0 + 7 - mon) / 7);
            }
            b'x' => {
                let _ = write!(
                    out,
                    "{:02}/{:02}/{:02}",
                    p.month,
                    p.day,
                    p.year.rem_euclid(100)
                );
            }
            b'X' => {
                let _ = write!(out, "{:02}:{:02}:{:02}", p.hour, p.min, p.sec);
            }
            b'y' => {
                let _ = write!(out, "{:02}", p.year.rem_euclid(100));
            }
            b'Y' => {
                let _ = write!(out, "{}", p.year);
            }
            b'Z' => out.push_str("UTC"),
            b'%' => out.push('%'),
            _ => return Err(format!("invalid conversion specifier '%{}'", c as char)),
        }
    }
    Ok(out)
}

fn n_date(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
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
    if fmt == b"*t" || fmt == b"!*t" {
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
    Ok(vec![lua.new_string(s.as_bytes())])
}

fn n_difftime(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let _ = lua;
    let to_num = |v: Value, i: usize| -> Result<f64, String> {
        match v {
            Value::Int(i) => Ok(i as f64),
            Value::Float(f) => Ok(f),
            Value::Str(_) => Err(format!(
                "bad argument #{i} to 'difftime' (number expected, got string)"
            )),
            other => Err(format!(
                "bad argument #{i} to 'difftime' (number expected, got {})",
                other.type_name()
            )),
        }
    };
    let t2 = to_num(arg(args, 0), 1)?;
    let t1 = to_num(arg(args, 1), 2)?;
    Ok(vec![Value::Float(t2 - t1)])
}

fn n_getenv(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
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

fn fs_call(
    lua: &mut Lua,
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

fn n_remove(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    fs_call(lua, args, "remove", |h, from, _| h.remove(&from))
}

fn n_rename(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    fs_call(lua, args, "rename", |h, from, to| h.rename(&from, &to))
}

fn n_tmpname(lua: &mut Lua, _args: &[Value]) -> Result<Vec<Value>, String> {
    let name = match lua.host.as_mut() {
        Some(h) => h.tmpname(),
        None => Err(HostError::new("os library has no host")),
    };
    match name {
        Ok(n) => Ok(vec![lua.new_string(n.as_bytes())]),
        Err(e) => Ok(err_return(lua, &e)),
    }
}

fn n_setlocale(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
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

fn n_exit(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
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
