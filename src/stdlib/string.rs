//! The `string` library. Pattern functions are backed by `crate::pattern`;
//! `gmatch`/`gsub` live in the Lua prelude on top of `string.find`.

use crate::pattern::{self, Capture};
use crate::value::{Value, fmt_g, fmt_number};
use crate::vm::{Intrinsic, Lua, NativeKind};

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
    // `format` is an intrinsic: `%s` may need to run `__tostring`, which
    // requires the frame machinery to suspend and resume.
    let format = lua.add_native_kind("format", NativeKind::Intrinsic(Intrinsic::Format));
    set_field(lua, st, "format", format);
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

/// PUC's `MAX_FORMAT`: `getformat` rejects spans that would need a longer
/// buffer than `MAX_FORMAT - 10`.
const MAX_FORMAT: usize = 32;

/// A parsed conversion specification: the raw `%...` text PUC's `checkformat`
/// inspects, plus the decoded flags/width/precision used for rendering.
pub(crate) struct FmtSpec {
    pub conv: u8,
    pub flags: Flags,
    pub width: usize,
    pub precision: Option<usize>,
    /// The raw specification, including `%` and the conversion character.
    pub form: Vec<u8>,
    /// Index in the format string just past this specification.
    pub next: usize,
}

/// PUC's `getformat`: span the accepted flags/width/precision characters, then
/// take the following conversion character. `pos` must point at a `%` that is
/// not part of a `%%` pair.
pub(crate) fn parse_spec(fmt: &[u8], pos: usize) -> Result<Option<FmtSpec>, String> {
    let rest = &fmt[pos + 1..];
    let span = rest
        .iter()
        .take_while(|&&c| matches!(c, b'-' | b'+' | b'#' | b'0' | b' ' | b'1'..=b'9' | b'.'))
        .count();
    if span + 1 >= MAX_FORMAT - 10 {
        return Err("invalid format (too long)".into());
    }
    let Some(&conv) = rest.get(span) else {
        return Err("invalid conversion to 'format'".into());
    };
    let mut form = Vec::with_capacity(span + 2);
    form.push(b'%');
    form.extend_from_slice(&rest[..span]);
    form.push(conv);
    let (flags, width, precision) = decode_spec(&form);
    Ok(Some(FmtSpec {
        conv,
        flags,
        width,
        precision,
        form,
        next: pos + 1 + span + 1,
    }))
}

/// Splits a validated specification into rendering parameters. `'0'` is
/// PUC's zero-padding flag; the remaining digit run is the width.
fn decode_spec(form: &[u8]) -> (Flags, usize, Option<usize>) {
    let body = &form[1..form.len() - 1];
    let mut flags = Flags::default();
    let mut i = 0;
    while i < body.len() {
        match body[i] {
            b'-' => flags.left = true,
            b'+' => flags.plus = true,
            b' ' => flags.space = true,
            b'#' => flags.alt = true,
            _ => break,
        }
        i += 1;
    }
    if i < body.len() && body[i] == b'0' {
        flags.zero = true;
        i += 1;
    }
    let mut width = 0usize;
    while i < body.len() && body[i].is_ascii_digit() {
        width = width * 10 + (body[i] - b'0') as usize;
        i += 1;
    }
    let mut precision = None;
    if i < body.len() && body[i] == b'.' {
        i += 1;
        let mut p = 0usize;
        while i < body.len() && body[i].is_ascii_digit() {
            p = p * 10 + (body[i] - b'0') as usize;
            i += 1;
        }
        precision = Some(p);
    }
    (flags, width, precision)
}

/// PUC's `checkformat`: after skipping the accepted flags and at most two
/// width/precision digits, the specification must end at an alphabetic
/// conversion character.
fn checkformat(form: &[u8], flags: &[u8], precision: bool) -> Result<(), String> {
    let mut i = 1;
    while i < form.len() && flags.contains(&form[i]) {
        i += 1;
    }
    if form.get(i) != Some(&b'0') {
        i = skip2digits(form, i);
        if form.get(i) == Some(&b'.') && precision {
            i += 1;
            i = skip2digits(form, i);
        }
    }
    match form.get(i) {
        Some(c) if c.is_ascii_alphabetic() => Ok(()),
        _ => Err(format!(
            "invalid conversion specification: '{}'",
            String::from_utf8_lossy(form)
        )),
    }
}

fn skip2digits(form: &[u8], mut i: usize) -> usize {
    if form.get(i).is_some_and(|c| c.is_ascii_digit()) {
        i += 1;
        if form.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
    }
    i
}

/// Renders one conversion, mirroring PUC's `str_format` ordering of argument
/// coercion and `checkformat` validation. `tostr` carries an already-resolved
/// `%s` value from `luaL_tolstring` (the `__tostring` result, or `None` when
/// the value has no metamethod).
pub(crate) fn render_spec(
    lua: &mut Lua,
    spec: &FmtSpec,
    v: Value,
    argi: usize,
    tostr: Option<Vec<u8>>,
) -> Result<Vec<u8>, String> {
    let form = &spec.form;
    let (flags, width, precision) = (spec.flags, spec.width, spec.precision);
    match spec.conv {
        b'c' => {
            checkformat(form, b"-", false)?;
            let n = want_int(v, argi)?;
            Ok(pad(vec![n as u8], width, flags, false))
        }
        b'd' | b'i' => {
            let n = want_int(v, argi)?;
            checkformat(form, b"-+0 ", true)?;
            let digits = int_digits(n.unsigned_abs(), 10, false, precision);
            let body = format!("{}{}", sign_prefix(n < 0, flags), digits);
            Ok(pad(
                body.into_bytes(),
                width,
                zero_for_precision(flags, precision),
                true,
            ))
        }
        b'u' => {
            let n = want_int(v, argi)?;
            checkformat(form, b"-0", true)?;
            let body = int_digits(n as u64, 10, false, precision);
            Ok(pad(
                body.into_bytes(),
                width,
                zero_for_precision(flags, precision),
                true,
            ))
        }
        b'o' => {
            let n = want_int(v, argi)?;
            checkformat(form, b"-#0", true)?;
            let mut body = int_digits(n as u64, 8, false, precision);
            if flags.alt && n != 0 && !body.starts_with('0') {
                body.insert(0, '0');
            }
            Ok(pad(
                body.into_bytes(),
                width,
                zero_for_precision(flags, precision),
                true,
            ))
        }
        b'x' => {
            let n = want_int(v, argi)?;
            checkformat(form, b"-#0", true)?;
            let digits = int_digits(n as u64, 16, false, precision);
            let body = if flags.alt && n != 0 {
                format!("0x{digits}")
            } else {
                digits
            };
            Ok(pad(
                body.into_bytes(),
                width,
                zero_for_precision(flags, precision),
                true,
            ))
        }
        b'X' => {
            let n = want_int(v, argi)?;
            checkformat(form, b"-#0", true)?;
            let digits = int_digits(n as u64, 16, true, precision);
            let body = if flags.alt && n != 0 {
                format!("0X{digits}")
            } else {
                digits
            };
            Ok(pad(
                body.into_bytes(),
                width,
                zero_for_precision(flags, precision),
                true,
            ))
        }
        b'f' => {
            let x = want_float(v, argi)?;
            checkformat(form, b"-+#0 ", true)?;
            let p = precision.unwrap_or(6);
            let mut body = format!("{:.*}", p, x.abs());
            if flags.alt && !body.contains('.') {
                body.push('.');
            }
            Ok(pad(
                format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes(),
                width,
                flags,
                true,
            ))
        }
        b'e' | b'E' => {
            let x = want_float(v, argi)?;
            checkformat(form, b"-+#0 ", true)?;
            let p = precision.unwrap_or(6);
            let mut body = format!("{:.*e}", p, x.abs());
            // Rust: "1.5e3" → C: "1.500000e+03"
            if let Some(epos) = body.find('e') {
                if flags.alt && !body[..epos].contains('.') {
                    body.insert(epos, '.');
                }
                let epos = body.find('e').unwrap();
                let exp: i32 = body[epos + 1..].parse().unwrap();
                body = format!(
                    "{}e{}{:02}",
                    &body[..epos],
                    if exp < 0 { '-' } else { '+' },
                    exp.abs()
                );
            }
            if spec.conv == b'E' {
                body = body.to_uppercase();
            }
            Ok(pad(
                format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes(),
                width,
                flags,
                true,
            ))
        }
        b'g' | b'G' => {
            let x = want_float(v, argi)?;
            checkformat(form, b"-+#0 ", true)?;
            let p = precision.unwrap_or(6).max(1);
            let mut body = fmt_g(x.abs(), p);
            if flags.alt && !body.contains('.') {
                // `#` forces a decimal point before the exponent (or at end).
                match body.find(['e', 'E']) {
                    Some(pos) => body.insert(pos, '.'),
                    None => body.push('.'),
                }
            }
            if spec.conv == b'G' {
                body = body.to_uppercase();
            }
            Ok(pad(
                format!("{}{}", sign_prefix(x.is_sign_negative(), flags), body).into_bytes(),
                width,
                flags,
                true,
            ))
        }
        b'a' | b'A' => {
            let x = want_float(v, argi)?;
            checkformat(form, b"-+#0 ", true)?;
            Ok(pad(
                format_hex_float(x, spec.conv == b'A', precision, flags),
                width,
                flags,
                true,
            ))
        }
        b'p' => {
            checkformat(form, b"-", false)?;
            Ok(pad(pointer_text(v).into_bytes(), width, flags, false))
        }
        b's' => {
            let mut s = match tostr {
                Some(bytes) => bytes,
                None => match v {
                    Value::Str(id) => lua.strings.get(id).to_vec(),
                    _ => lua.tostring_default(v).into_bytes(),
                },
            };
            if form.len() == 2 {
                return Ok(s);
            }
            // PUC measures the width with `strlen`, so embedded zeros are
            // rejected once any modifier is present.
            if s.contains(&0) {
                return Err(format!(
                    "bad argument #{argi} to 'format' (string contains zeros)"
                ));
            }
            checkformat(form, b"-", true)?;
            if precision.is_none() && s.len() >= 100 {
                return Ok(s);
            }
            if let Some(p) = precision {
                s.truncate(p);
            }
            Ok(pad(s, width, flags, false))
        }
        b'q' => {
            if form.len() != 2 {
                return Err("specifier '%q' cannot have modifiers".into());
            }
            let out = match v {
                Value::Str(id) => {
                    let bytes = lua.strings.get(id).to_vec();
                    let mut out = vec![b'"'];
                    for (i, &b) in bytes.iter().enumerate() {
                        match b {
                            // Quote these directly: C's `addquoted` emits a
                            // backslash followed by the byte itself, so a
                            // newline becomes backslash + an actual newline.
                            b'"' | b'\\' | b'\n' => {
                                out.push(b'\\');
                                out.push(b);
                            }
                            _ if b < 32 || b == 127 => {
                                // A decimal escape must not swallow a
                                // following digit, so PUC zero-pads to three
                                // digits then.
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
                // Numbers are written as a Lua literal that scans back
                // exactly: the most-negative integer needs hex (its magnitude
                // overflows), infinities need a value that parses to them, NaN
                // has no numeral.
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
            };
            Ok(out)
        }
        _ => Err(format!(
            "invalid conversion '{}' to 'format'",
            String::from_utf8_lossy(form)
        )),
    }
}

#[derive(Default, Clone, Copy)]
pub(crate) struct Flags {
    pub(crate) left: bool,
    pub(crate) zero: bool,
    pub(crate) plus: bool,
    pub(crate) space: bool,
    pub(crate) alt: bool,
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

/// C's `'0'` flag is ignored once a precision is given.
fn zero_for_precision(flags: Flags, precision: Option<usize>) -> Flags {
    let mut f = flags;
    if precision.is_some() {
        f.zero = false;
    }
    f
}

/// Integer digits for `%d`/`%i`/`%u`/`%o`/`%x`/`%X`: `precision` is the
/// minimum digit count (zero value with precision 0 renders no digits).
fn int_digits(mag: u64, radix: u32, upper: bool, precision: Option<usize>) -> String {
    let mut s = match radix {
        8 => format!("{mag:o}"),
        16 if upper => format!("{mag:X}"),
        16 => format!("{mag:x}"),
        _ => mag.to_string(),
    };
    if mag == 0 && precision == Some(0) {
        s.clear();
    }
    if let Some(p) = precision
        && s.len() < p
    {
        s.insert_str(0, &"0".repeat(p - s.len()));
    }
    s
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
