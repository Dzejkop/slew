//! The `utf8` library (a port of PUC's lutf8lib.c).
//!
//! Handles the full Lua 5.4 extended UTF-8 range: code points up to
//! `MAXUTF` (0x7FFFFFFF) are decodable/encodable, and the `lax` flag controls
//! the strict checks (overlong forms are always rejected; the strict mode
//! additionally rejects surrogates and values above 0x10FFFF).

use crate::value::{Value, float_to_exact_int};
use crate::vm::Lua;

use super::{arg, check_bytes, set_field};

const MAXUNICODE: u32 = 0x10_FFFF;
const MAXUTF: u32 = 0x7FFF_FFFF;
/// `UTF8PATT` from lutf8lib.c: `"[\0-\x7F\xC2-\xFD][\x80-\xBF]*"`.
const CHARPATTERN: &[u8] = b"[\x00-\x7F\xC2-\xFD][\x80-\xBF]*";
/// Minimum value for each sequence length (index 0 forces an error for a
/// lone continuation byte), from lutf8lib.c.
const LIMITS: [u32; 6] = [u32::MAX, 0x80, 0x800, 0x1_0000, 0x20_0000, 0x400_0000];

pub fn install<C>(lua: &mut Lua<C>) {
    let lib = lua.new_table();
    lua.set_global("utf8", lib);
    for (name, f) in [
        ("char", n_char as crate::vm::NativeFn<C>),
        ("codepoint", n_codepoint),
        ("len", n_len),
        ("offset", n_offset),
        ("codes", n_codes),
    ] {
        let v = lua.add_native(name, f);
        set_field(lua, lib, name, v);
    }
    let pat = lua.new_string(CHARPATTERN);
    set_field(lua, lib, "charpattern", pat);
}

/// `luaL_checkinteger`-ish: integers, floats with an exact integer value, and
/// numeric strings; everything else is an argument error.
fn check_integer<C>(lua: &Lua<C>, args: &[Value], i: usize, who: &str) -> Result<i64, String> {
    let v = arg(args, i);
    if let Some(n) = as_integer(lua, v) {
        return Ok(n);
    }
    let numeric = match v {
        Value::Int(_) | Value::Float(_) => true,
        Value::Str(id) => super::parse_number(lua.strings.get(id)).is_some(),
        _ => false,
    };
    if numeric {
        Err(format!(
            "bad argument #{} to '{who}' (number has no integer representation)",
            i + 1
        ))
    } else {
        Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        ))
    }
}

fn as_integer<C>(lua: &Lua<C>, v: Value) -> Option<i64> {
    match v {
        Value::Int(n) => Some(n),
        Value::Float(f) => float_to_exact_int(f),
        Value::Str(id) => match super::parse_number(lua.strings.get(id)) {
            Some(Value::Int(n)) => Some(n),
            Some(Value::Float(f)) => float_to_exact_int(f),
            _ => None,
        },
        _ => None,
    }
}

/// `lua_tointeger`-ish: like `as_integer` but non-numbers silently become 0.
fn to_integer<C>(lua: &Lua<C>, v: Value) -> i64 {
    as_integer(lua, v).unwrap_or(0)
}

fn opt_integer<C>(
    lua: &Lua<C>,
    args: &[Value],
    i: usize,
    default: i64,
    who: &str,
) -> Result<i64, String> {
    if arg(args, i) == Value::Nil {
        Ok(default)
    } else {
        check_integer(lua, args, i, who)
    }
}

/// PUC's `u_posrelat`: translate a relative string position (negative counts
/// back from the end; too-negative clamps to 0).
fn u_posrelat(pos: i64, len: usize) -> i64 {
    if pos >= 0 {
        pos
    } else if pos.unsigned_abs() > len as u64 {
        0
    } else {
        len as i64 + pos + 1
    }
}

fn is_cont(b: u8) -> bool {
    b & 0xC0 == 0x80
}

fn is_cont_at(s: &[u8], i: usize) -> bool {
    s.get(i).is_some_and(|&b| is_cont(b))
}

/// Port of lutf8lib.c's `utf8_decode`: returns the code point and the number
/// of bytes consumed, or `None` for an invalid sequence.
fn utf8_decode(s: &[u8], strict: bool) -> Option<(u32, usize)> {
    let c0 = *s.first()? as u32;
    let (res, count) = if c0 < 0x80 {
        (c0, 0)
    } else if c0 >= 0xFE {
        return None;
    } else {
        let mut count = 0usize;
        let mut c = c0;
        let mut res = 0u32;
        while c & 0x40 != 0 {
            let cc = *s.get(count + 1)? as u32;
            if cc & 0xC0 != 0x80 {
                return None;
            }
            res = (res << 6) | (cc & 0x3F);
            c <<= 1;
            count += 1;
        }
        res |= (c & 0x7F) << (count * 5);
        if res > MAXUTF || res < LIMITS[count] {
            return None;
        }
        (res, count)
    };
    if strict && (res > MAXUNICODE || (0xD800..=0xDFFF).contains(&res)) {
        return None;
    }
    Some((res, count + 1))
}

/// Port of `luaO_utf8esc`: encodes `x` (already checked to be <= MAXUTF) in
/// Lua's extended UTF-8 (1 to 6 bytes).
fn utf8_encode(x: u32, out: &mut Vec<u8>) {
    if x < 0x80 {
        out.push(x as u8);
        return;
    }
    let mut bytes = [0u8; 6];
    let mut n = 0usize;
    let mut mfb: u32 = 0x3F;
    let mut x = x;
    loop {
        bytes[n] = (0x80 | (x & 0x3F)) as u8;
        n += 1;
        x >>= 6;
        mfb >>= 1;
        if x <= mfb {
            break;
        }
    }
    bytes[n] = ((!mfb << 1) | x) as u8;
    for i in (0..=n).rev() {
        out.push(bytes[i]);
    }
}

fn n_char<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(args.len());
    for i in 0..args.len() {
        let code = check_integer(lua, args, i, "char")? as u64;
        if code > MAXUTF as u64 {
            return Err(format!(
                "bad argument #{} to 'char' (value out of range)",
                i + 1
            ));
        }
        utf8_encode(code as u32, &mut out);
    }
    Ok(vec![lua.new_string(&out)])
}

fn n_codepoint<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = check_bytes(lua, args, 0, "codepoint")?;
    let len = s.len();
    let posi = u_posrelat(opt_integer(lua, args, 1, 1, "codepoint")?, len);
    let pose = u_posrelat(opt_integer(lua, args, 2, posi, "codepoint")?, len);
    let lax = arg(args, 3).truthy();
    if posi < 1 {
        return Err("bad argument #2 to 'codepoint' (out of bounds)".into());
    }
    if pose > len as i64 {
        return Err("bad argument #3 to 'codepoint' (out of bounds)".into());
    }
    if posi > pose {
        return Ok(vec![]);
    }
    let mut out = Vec::new();
    let mut pos = (posi - 1) as usize;
    let end = pose as usize;
    while pos < end {
        match utf8_decode(&s[pos..], !lax) {
            Some((code, n)) => {
                out.push(Value::Int(code as i64));
                pos += n;
            }
            None => return Err("invalid UTF-8 code".into()),
        }
    }
    Ok(out)
}

fn n_len<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = check_bytes(lua, args, 0, "len")?;
    let len = s.len();
    let mut posi = u_posrelat(opt_integer(lua, args, 1, 1, "len")?, len);
    let mut posj = u_posrelat(opt_integer(lua, args, 2, -1, "len")?, len);
    let lax = arg(args, 3).truthy();
    posi -= 1;
    if posi < 0 || posi > len as i64 {
        return Err("bad argument #2 to 'len' (initial position out of bounds)".into());
    }
    posj -= 1;
    if posj >= len as i64 {
        return Err("bad argument #3 to 'len' (final position out of bounds)".into());
    }
    let mut n: i64 = 0;
    while posi <= posj {
        match utf8_decode(&s[posi as usize..], !lax) {
            Some((_, consumed)) => {
                posi += consumed as i64;
                n += 1;
            }
            None => return Ok(vec![Value::Nil, Value::Int(posi + 1)]),
        }
    }
    Ok(vec![Value::Int(n)])
}

fn n_offset<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = check_bytes(lua, args, 0, "offset")?;
    let len = s.len();
    let mut n = check_integer(lua, args, 1, "offset")?;
    let default_i = if n >= 0 { 1 } else { len as i64 + 1 };
    let mut posi = u_posrelat(opt_integer(lua, args, 2, default_i, "offset")?, len);
    posi -= 1;
    if posi < 0 || posi > len as i64 {
        return Err("bad argument #3 to 'offset' (position out of bounds)".into());
    }
    if n == 0 {
        while posi > 0 && is_cont_at(&s, posi as usize) {
            posi -= 1;
        }
    } else {
        if is_cont_at(&s, posi as usize) {
            return Err("initial position is a continuation byte".into());
        }
        if n < 0 {
            while n < 0 && posi > 0 {
                loop {
                    posi -= 1;
                    if posi == 0 || !is_cont_at(&s, posi as usize) {
                        break;
                    }
                }
                n += 1;
            }
        } else {
            n -= 1;
            while n > 0 && posi < len as i64 {
                loop {
                    posi += 1;
                    if !is_cont_at(&s, posi as usize) {
                        break;
                    }
                }
                n -= 1;
            }
        }
    }
    if n == 0 {
        Ok(vec![Value::Int(posi + 1)])
    } else {
        Ok(vec![Value::Nil])
    }
}

fn n_codes<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let s = check_bytes(lua, args, 0, "codes")?;
    let lax = arg(args, 1).truthy();
    if is_cont_at(&s, 0) {
        return Err("bad argument #1 to 'codes' (invalid UTF-8 code)".into());
    }
    let f = lua.add_native(
        "codes_iterator",
        if lax { n_iter_lax } else { n_iter_strict },
    );
    Ok(vec![f, lua.new_string(&s), Value::Int(0)])
}

fn n_iter_strict<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    iter_aux(lua, args, true)
}

fn n_iter_lax<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    iter_aux(lua, args, false)
}

fn iter_aux<C>(lua: &mut Lua<C>, args: &[Value], strict: bool) -> Result<Vec<Value>, String> {
    let s = check_bytes(lua, args, 0, "codes")?;
    let len = s.len();
    let control = to_integer(lua, arg(args, 1));
    let mut n = control as u64;
    if n < len as u64 {
        while is_cont_at(&s, n as usize) {
            n += 1;
        }
    }
    if n >= len as u64 {
        return Ok(vec![]);
    }
    let pos = n as usize;
    match utf8_decode(&s[pos..], strict) {
        Some((code, consumed)) if !is_cont_at(&s, pos + consumed) => {
            Ok(vec![Value::Int(n as i64 + 1), Value::Int(code as i64)])
        }
        _ => Err("invalid UTF-8 code".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_strict_and_lax() {
        assert_eq!(utf8_decode(b"a", true), Some((0x61, 1)));
        assert_eq!(utf8_decode("\u{e1}".as_bytes(), true), Some((0xE1, 2)));
        assert_eq!(utf8_decode(b"\xC0\x80", false), None); // overlong
        assert_eq!(utf8_decode(b"\xED\xA0\x80", true), None); // surrogate
        assert_eq!(utf8_decode(b"\xED\xA0\x80", false), Some((0xD800, 3)));
        assert_eq!(utf8_decode(b"\xF4\x90\x80\x80", true), None); // > 0x10FFFF
        assert_eq!(utf8_decode(b"\x80", false), None);
    }

    #[test]
    fn encode_roundtrip() {
        let mut buf = Vec::new();
        utf8_encode(0x0010_FFFF, &mut buf);
        assert_eq!(buf, vec![0xF4, 0x8F, 0xBF, 0xBF]);
        buf.clear();
        utf8_encode(0x7FFF_FFFF, &mut buf);
        assert_eq!(buf, vec![0xFD, 0xBF, 0xBF, 0xBF, 0xBF, 0xBF]);
    }

    #[test]
    fn charpattern_is_puc_bytes() {
        assert_eq!(CHARPATTERN.len(), 14);
    }
}
