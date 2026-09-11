//! The `string` library. Pattern functions are backed by `crate::pattern`;
//! `gmatch`/`gsub` live in the Lua prelude on top of `string.find`.

use crate::pattern::{self, Capture};
use crate::value::{Value, fmt_g, fmt_number};
use crate::vm::Lua;

use super::string_pack::{n_pack, n_packsize, n_unpack};
use super::{arg, set_field};

pub fn install(lua: &mut Lua) {
    let st = lua.new_table();
    lua.set_global("string", st);
    for (name, f) in [
        ("len", n_len as crate::vm::NativeFn),
        ("sub", n_sub),
        ("upper", n_upper),
        ("lower", n_lower),
        ("rep", n_rep),
        ("reverse", n_reverse),
        ("byte", n_byte),
        ("char", n_char),
        ("format", n_format),
        ("find", n_find),
        ("match", n_match),
        ("pack", n_pack),
        ("unpack", n_unpack),
        ("packsize", n_packsize),
        ("dump", super::dump::n_dump),
    ] {
        let v = lua.add_native(name, f);
        set_field(lua, st, name, v);
    }
    // all strings share a metatable with __index = string, enabling
    // ("x"):upper() method syntax
    let meta = lua.new_table();
    set_field(lua, meta, "__index", st);
    let Value::Table(meta_id) = meta else {
        unreachable!()
    };
    lua.string_meta = Some(meta_id);
}

/// String argument with Lua's number→string coercion.
fn arg_str(lua: &Lua, args: &[Value], i: usize, who: &str) -> Result<Vec<u8>, String> {
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

fn arg_int(args: &[Value], i: usize, who: &str) -> Result<Option<i64>, String> {
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

/// Converts a 1-based (possibly negative) string index to a 0-based offset,
/// per Lua's relative-index rules.
fn str_index(i: i64, len: usize, default_for_zero: usize) -> usize {
    if i > 0 {
        (i as usize - 1).min(len)
    } else if i == 0 {
        default_for_zero
    } else {
        len.saturating_sub(i.unsigned_abs() as usize)
    }
}

fn n_len(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "len")?;
    Ok(vec![Value::Int(s.len() as i64)])
}

fn n_sub(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "sub")?;
    let len = s.len();
    let i = arg_int(args, 1, "sub")?.unwrap_or(1);
    let j = arg_int(args, 2, "sub")?.unwrap_or(-1);
    let start = str_index(i, len, 0);
    // j is inclusive: convert to exclusive end
    let end = if j >= 0 {
        (j as usize).min(len)
    } else {
        len.saturating_sub(j.unsigned_abs() as usize - 1)
    };
    let out = if start < end { &s[start..end] } else { &[][..] };
    Ok(vec![lua.new_string(out)])
}

fn n_upper(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "upper")?.to_ascii_uppercase();
    Ok(vec![lua.new_string(&s)])
}

fn n_lower(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "lower")?.to_ascii_lowercase();
    Ok(vec![lua.new_string(&s)])
}

fn n_rep(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "rep")?;
    let n = arg_int(args, 1, "rep")?.unwrap_or(0);
    let sep = match arg(args, 2) {
        Value::Nil => Vec::new(),
        _ => arg_str(lua, args, 2, "rep")?,
    };
    if n <= 0 {
        return Ok(vec![lua.new_string(b"")]);
    }
    // PUC errors when (len + seplen) * n exceeds MAXSIZE; compute the exact
    // total (len*n + seplen*(n-1)) without overflowing the host usize.
    let per = s.len().checked_add(sep.len());
    let fits = per
        .and_then(|p| p.checked_mul(n as usize))
        .is_some_and(|t| t <= i32::MAX as usize);
    if !fits {
        return Err("resulting string too large".into());
    }
    let total = s.len() * n as usize + sep.len() * (n as usize - 1);
    let mut out = Vec::with_capacity(total);
    for i in 0..n {
        if i > 0 {
            out.extend_from_slice(&sep);
        }
        out.extend_from_slice(&s);
    }
    Ok(vec![lua.new_string(&out)])
}

fn n_reverse(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut s = arg_str(lua, args, 0, "reverse")?;
    s.reverse();
    Ok(vec![lua.new_string(&s)])
}

fn n_byte(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "byte")?;
    let i = arg_int(args, 1, "byte")?.unwrap_or(1);
    let j = arg_int(args, 2, "byte")?.unwrap_or(i);
    let len = s.len();
    let start = str_index(i, len, 0);
    let end = if j >= 0 {
        (j as usize).min(len)
    } else {
        len.saturating_sub(j.unsigned_abs() as usize - 1)
    };
    Ok(s.get(start..end)
        .unwrap_or(&[])
        .iter()
        .map(|&b| Value::Int(b as i64))
        .collect())
}

fn n_char(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(args.len());
    for (i, _) in args.iter().enumerate() {
        let b = arg_int(args, i, "char")?.unwrap_or(-1);
        if !(0..=255).contains(&b) {
            return Err(format!(
                "bad argument #{} to 'char' (value out of range)",
                i + 1
            ));
        }
        out.push(b as u8);
    }
    Ok(vec![lua.new_string(&out)])
}

// ---- find / match ----

fn capture_value(lua: &mut Lua, s: &[u8], c: Capture) -> Value {
    match c {
        Capture::Span(a, b) => lua.new_string(&s[a..b]),
        Capture::Pos(p) => Value::Int(p as i64 + 1),
    }
}

fn n_find(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "find")?;
    let pat = arg_str(lua, args, 1, "find")?;
    let init = match arg_int(args, 2, "find")?.unwrap_or(1) {
        i if i > 0 => i as usize - 1,
        0 => 0,
        i => s.len().saturating_sub(i.unsigned_abs() as usize),
    };
    if init > s.len() {
        return Ok(vec![Value::Nil]);
    }
    let plain = arg(args, 3).truthy();
    if plain {
        // plain substring search
        let found = if pat.is_empty() {
            Some(init)
        } else {
            s[init..]
                .windows(pat.len())
                .position(|w| w == &pat[..])
                .map(|p| p + init)
        };
        return Ok(match found {
            Some(p) => vec![Value::Int(p as i64 + 1), Value::Int((p + pat.len()) as i64)],
            None => vec![Value::Nil],
        });
    }
    match pattern::find(&s, &pat, init)? {
        None => Ok(vec![Value::Nil]),
        Some(m) => {
            let mut out = vec![Value::Int(m.start as i64 + 1), Value::Int(m.end as i64)];
            for c in m.captures {
                out.push(capture_value(lua, &s, c));
            }
            Ok(out)
        }
    }
}

fn n_match(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = arg_str(lua, args, 0, "match")?;
    let pat = arg_str(lua, args, 1, "match")?;
    let init = match arg_int(args, 2, "match")?.unwrap_or(1) {
        i if i > 0 => i as usize - 1,
        0 => 0,
        i => s.len().saturating_sub(i.unsigned_abs() as usize),
    };
    if init > s.len() {
        return Ok(vec![Value::Nil]);
    }
    match pattern::find(&s, &pat, init)? {
        None => Ok(vec![Value::Nil]),
        Some(m) => {
            if m.captures.is_empty() {
                Ok(vec![lua.new_string(&s[m.start..m.end])])
            } else {
                Ok(m.captures
                    .into_iter()
                    .map(|c| capture_value(lua, &s, c))
                    .collect())
            }
        }
    }
}

// ---- format ----

fn n_format(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let fmt = arg_str(lua, args, 0, "format")?;
    let mut out: Vec<u8> = Vec::new();
    let mut argi = 1;
    let mut it = fmt.iter().copied().peekable();
    while let Some(c) = it.next() {
        if c != b'%' {
            out.push(c);
            continue;
        }
        if it.peek() == Some(&b'%') {
            it.next();
            out.push(b'%');
            continue;
        }
        // parse spec: flags, width, .precision, conversion
        let mut spec: Vec<u8> = vec![b'%'];
        let mut flags = Flags::default();
        while let Some(&c) = it.peek() {
            match c {
                b'-' => flags.left = true,
                b'0' => flags.zero = true,
                b'+' => flags.plus = true,
                b' ' => flags.space = true,
                b'#' => flags.alt = true,
                _ => break,
            }
            spec.push(c);
            it.next();
        }
        let mut width = 0usize;
        while let Some(&c) = it.peek() {
            if c.is_ascii_digit() {
                width = width * 10 + (c - b'0') as usize;
                spec.push(c);
                it.next();
            } else {
                break;
            }
        }
        let mut precision: Option<usize> = None;
        if it.peek() == Some(&b'.') {
            spec.push(b'.');
            it.next();
            let mut p = 0usize;
            while let Some(&c) = it.peek() {
                if c.is_ascii_digit() {
                    p = p * 10 + (c - b'0') as usize;
                    spec.push(c);
                    it.next();
                } else {
                    break;
                }
            }
            precision = Some(p);
        }
        let conv = it.next().ok_or("invalid conversion to 'format'")?;
        spec.push(conv);
        // PUC's `checkformat`: '%p' accepts only the '-' flag and no precision.
        if conv == b'p'
            && (flags.zero || flags.plus || flags.space || flags.alt || precision.is_some())
        {
            return Err(format!(
                "invalid conversion specification: '{}'",
                String::from_utf8_lossy(&spec)
            ));
        }
        let v = arg(args, argi);
        argi += 1;
        let piece = format_one(lua, conv, v, flags, width, precision, argi)?;
        out.extend_from_slice(&piece);
    }
    Ok(vec![lua.new_string(&out)])
}

#[derive(Default, Clone, Copy)]
struct Flags {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
}

fn pad(s: Vec<u8>, width: usize, flags: Flags, numeric: bool) -> Vec<u8> {
    if s.len() >= width {
        return s;
    }
    let fill = width - s.len();
    let mut out = Vec::with_capacity(width);
    if flags.left {
        out.extend_from_slice(&s);
        out.extend(std::iter::repeat_n(b' ', fill));
    } else if flags.zero && numeric {
        // zero-pad after any sign
        let signed = s
            .first()
            .is_some_and(|&c| c == b'-' || c == b'+' || c == b' ');
        if signed {
            out.push(s[0]);
            out.extend(std::iter::repeat_n(b'0', fill));
            out.extend_from_slice(&s[1..]);
        } else {
            out.extend(std::iter::repeat_n(b'0', fill));
            out.extend_from_slice(&s);
        }
    } else {
        out.extend(std::iter::repeat_n(b' ', fill));
        out.extend_from_slice(&s);
    }
    out
}

fn want_int(v: Value, argi: usize) -> Result<i64, String> {
    match v {
        Value::Int(i) => Ok(i),
        Value::Float(f) => crate::value::float_to_exact_int(f).ok_or_else(|| {
            format!("bad argument #{argi} to 'format' (number has no integer representation)")
        }),
        _ => Err(format!(
            "bad argument #{argi} to 'format' (number expected, got {})",
            v.type_name()
        )),
    }
}

fn want_float(v: Value, argi: usize) -> Result<f64, String> {
    match v {
        Value::Int(i) => Ok(i as f64),
        Value::Float(f) => Ok(f),
        _ => Err(format!(
            "bad argument #{argi} to 'format' (number expected, got {})",
            v.type_name()
        )),
    }
}

fn sign_prefix(neg: bool, flags: Flags) -> &'static str {
    if neg {
        "-"
    } else if flags.plus {
        "+"
    } else if flags.space {
        " "
    } else {
        ""
    }
}

fn format_one(
    lua: &Lua,
    conv: u8,
    v: Value,
    flags: Flags,
    width: usize,
    precision: Option<usize>,
    argi: usize,
) -> Result<Vec<u8>, String> {
    let s: Vec<u8> = match conv {
        b'd' | b'i' => {
            let n = want_int(v, argi)?;
            let body = n.unsigned_abs().to_string();
            format!("{}{}", sign_prefix(n < 0, flags), body).into_bytes()
        }
        b'u' => want_int(v, argi)?.to_string().into_bytes(),
        b'x' => {
            let n = want_int(v, argi)? as u64;
            let body = format!("{n:x}");
            if flags.alt && n != 0 {
                format!("0x{body}")
            } else {
                body
            }
            .into_bytes()
        }
        b'X' => {
            let n = want_int(v, argi)? as u64;
            let body = format!("{n:X}");
            if flags.alt && n != 0 {
                format!("0X{body}")
            } else {
                body
            }
            .into_bytes()
        }
        b'o' => format!("{:o}", want_int(v, argi)? as u64).into_bytes(),
        b'c' => {
            let n = want_int(v, argi)?;
            vec![n as u8]
        }
        b'f' | b'F' => {
            let x = want_float(v, argi)?;
            let p = precision.unwrap_or(6);
            let body = format!("{:.*}", p, x.abs());
            format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes()
        }
        b'e' | b'E' => {
            let x = want_float(v, argi)?;
            let p = precision.unwrap_or(6);
            let mut body = format!("{:.*e}", p, x.abs());
            // Rust: "1.5e3" → C: "1.500000e+03"
            if let Some(epos) = body.find('e') {
                let exp: i32 = body[epos + 1..].parse().unwrap();
                body = format!(
                    "{}e{}{:02}",
                    &body[..epos],
                    if exp < 0 { '-' } else { '+' },
                    exp.abs()
                );
            }
            if conv == b'E' {
                body = body.to_uppercase();
            }
            format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes()
        }
        b'g' | b'G' => {
            let x = want_float(v, argi)?;
            let p = precision.unwrap_or(6).max(1);
            let mut body = fmt_g(x.abs(), p);
            if conv == b'G' {
                body = body.to_uppercase();
            }
            format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes()
        }
        b'a' | b'A' => {
            let x = want_float(v, argi)?;
            format_hex_float(x, conv == b'A', precision, flags)
        }
        b'p' => pointer_text(v).into_bytes(),
        b's' => {
            let mut s = match v {
                Value::Str(id) => lua.strings.get(id).to_vec(),
                _ => lua.display_value(v).into_bytes(),
            };
            // PUC only accepts embedded zeros for the unmodified `%s`; with
            // any flags/width/precision the width is measured with `strlen`,
            // so a NUL would silently truncate the value.
            let modified = flags.left
                || flags.zero
                || flags.plus
                || flags.space
                || flags.alt
                || width > 0
                || precision.is_some();
            if modified && s.contains(&0) {
                return Err(format!(
                    "bad argument #{argi} to 'format' (string contains zeros)"
                ));
            }
            if let Some(p) = precision {
                s.truncate(p);
            }
            s
        }
        b'q' => match v {
            Value::Str(id) => {
                let bytes = lua.strings.get(id).to_vec();
                let mut out = vec![b'"'];
                for (i, &b) in bytes.iter().enumerate() {
                    match b {
                        // Quote these directly: C's `addquoted` emits a
                        // backslash followed by the byte itself, so a newline
                        // becomes backslash + an actual newline.
                        b'"' | b'\\' | b'\n' => {
                            out.push(b'\\');
                            out.push(b);
                        }
                        _ if b < 32 || b == 127 => {
                            // A decimal escape must not swallow a following
                            // digit, so PUC zero-pads to three digits then.
                            if bytes.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                                out.extend_from_slice(format!("\\{b:03}").as_bytes());
                            } else {
                                out.extend_from_slice(format!("\\{b}").as_bytes());
                            }
                        }
                        _ => out.push(b),
                    }
                }
                out.push(b'"');
                out
            }
            // Numbers are written as a Lua literal that scans back exactly:
            // the most-negative integer needs hex (its magnitude overflows),
            // infinities need a value that parses to them, NaN has no numeral.
            Value::Int(n) if n == i64::MIN => format!("0x{:x}", n as u64).into_bytes(),
            Value::Int(n) => n.to_string().into_bytes(),
            Value::Float(x) if x == f64::INFINITY => b"1e9999".to_vec(),
            Value::Float(x) if x == f64::NEG_INFINITY => b"-1e9999".to_vec(),
            Value::Float(x) if x.is_nan() => b"(0/0)".to_vec(),
            Value::Float(x) => format_hex_float(x, false, None, Flags::default()),
            Value::Nil => b"nil".to_vec(),
            Value::Bool(b) => b.to_string().into_bytes(),
            _ => {
                return Err(format!(
                    "bad argument #{argi} to 'format' (value has no literal form)"
                ));
            }
        },
        c => return Err(format!("invalid conversion '%{}' to 'format'", c as char)),
    };
    let numeric = !matches!(conv, b's' | b'q' | b'c' | b'p');
    Ok(pad(s, width, flags, numeric))
}

/// `string.format("%p", v)`: nil/booleans/numbers have no address in Lua and
/// render as `(null)` (as PUC does when `lua_topointer` is NULL). Everything
/// else is a stable, type-tagged handle rendered as hex. For strings this is
/// the *object* identity, so interned short strings share an address while
/// distinct runtime long strings do not.
fn pointer_text(v: Value) -> String {
    let tag = match v {
        Value::Str(id) => 0x1000_0000u64 + id.obj.0 as u64,
        Value::Table(id) => 0x2000_0000u64 + id.0 as u64,
        Value::Native(id) => 0x3000_0000u64 + id.0 as u64,
        Value::Closure(id) => 0x4000_0000u64 + id.0 as u64,
        Value::Thread(id) => 0x5000_0000u64 + id.0 as u64,
        Value::Userdata(id) => 0x6000_0000u64 + id.0 as u64,
        _ => return "(null)".to_string(),
    };
    format!("0x{tag:x}")
}

/// C's `%a`/`%A`: a hex float in the form `[-]0x<lead>.<frac>p<exp>`.
/// A default precision keeps the exact 52-bit mantissa (trailing zeros
/// trimmed, the fractional part omitted when zero), matching ISO C/glibc.
fn format_hex_float(x: f64, upper: bool, precision: Option<usize>, flags: Flags) -> Vec<u8> {
    if x.is_nan() {
        let mut s = String::new();
        if x.is_sign_negative() {
            s.push('-');
        }
        s.push_str(if upper { "NAN" } else { "nan" });
        return s.into_bytes();
    }
    if x.is_infinite() {
        let mut s = String::from(sign_prefix(x.is_sign_negative(), flags));
        s.push_str(if upper { "INF" } else { "inf" });
        return s.into_bytes();
    }
    let mut s = String::from(sign_prefix(x.is_sign_negative(), flags));
    s.push_str(if upper { "0X" } else { "0x" });
    let bits = x.abs().to_bits();
    let raw_exp = ((bits >> 52) & 0x7ff) as i64;
    let frac = bits & ((1u64 << 52) - 1);
    let zero = raw_exp == 0 && frac == 0;
    let mut lead: u64 = if raw_exp == 0 { 0 } else { 1 };
    let exp: i64 = if zero {
        0
    } else if raw_exp == 0 {
        -1022
    } else {
        raw_exp - 1023
    };
    let digits: String = if zero {
        precision.map_or_else(String::new, |p| "0".repeat(p))
    } else {
        match precision {
            None => {
                let mut d = format!("{frac:013x}");
                while d.ends_with('0') {
                    d.pop();
                }
                d
            }
            Some(0) => {
                // round the fraction into the leading digit
                let half = 1u64 << 51;
                if frac > half || (frac == half && lead & 1 == 1) {
                    lead += 1;
                }
                String::new()
            }
            Some(p) if p <= 28 => {
                let keep = p * 4;
                let kept: u128 = if keep >= 52 {
                    (frac as u128) << (keep - 52)
                } else {
                    let shift = 52 - keep;
                    let k = frac >> shift;
                    let rem = frac & ((1u64 << shift) - 1);
                    let half = 1u64 << (shift - 1);
                    let up = rem > half || (rem == half && (k & 1) == 1);
                    let r = k as u128 + up as u128;
                    if r >> keep != 0 {
                        lead += 1;
                        0
                    } else {
                        r
                    }
                };
                format!("{kept:0width$x}", width = p)
            }
            Some(p) => {
                // Excessive precision: exact digits plus zero padding.
                let mut d = format!("{frac:013x}");
                d.push_str(&"0".repeat(p.saturating_sub(13)));
                d
            }
        }
    };
    s.push(char::from_digit(lead as u32, 16).unwrap());
    if !digits.is_empty() {
        s.push('.');
        s.push_str(&digits);
    }
    s.push(if upper { 'P' } else { 'p' });
    s.push(if exp < 0 { '-' } else { '+' });
    s.push_str(&exp.abs().to_string());
    if upper {
        s = s.to_uppercase();
    }
    s.into_bytes()
}
