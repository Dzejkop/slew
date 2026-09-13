//! `string.pack`, `string.unpack`, and `string.packsize`, a faithful port of
//! the packing machinery in PUC's `lstrlib.c` (Lua 5.4). All sizes are the
//! native ones for the platforms slew targets (LP64: `char` 1, `short` 2,
//! `int` 4, `long`/`lua_Integer`/`size_t`/`lua_Number`/`double` 8, `float` 4).

use std::os::raw::{c_long, c_short};

use crate::value::{Value, float_to_exact_int};
use crate::vm::Lua;

use super::{arg, parse_number};

const NATIVE_LITTLE: bool = cfg!(target_endian = "little");
const SZINT: usize = 8; // sizeof(lua_Integer)
const MAXINTSIZE: usize = 16; // PUC's maximum integral size
/// `MAXSIZE` from `lstrlib.c`: `sizeof(size_t) >= sizeof(int) ? INT_MAX : ...`
/// On every platform slew targets `size_t` is at least as wide as `int`.
const MAXSIZE: usize = i32::MAX as usize;

#[repr(C)]
union MaxAlign {
    n: f64,
    i: i64,
    l: c_long,
    p: *const u8,
}

/// Native maximum alignment (`offsetof(struct { char; union { LUAI_MAXALIGN; } })`
/// in PUC), normally 8 on 64-bit targets.
const MAXALIGN: usize = std::mem::align_of::<MaxAlign>();

#[derive(Clone, Copy, PartialEq, Eq)]
enum KOption {
    Kint,
    Kuint,
    Kfloat,
    Knumber,
    Kdouble,
    Kchar,
    Kstring,
    Kzstr,
    Kpadding,
    Kpaddalign,
    Knop,
}

struct Header {
    islittle: bool,
    maxalign: usize,
    who: &'static str,
}

impl Header {
    fn new(who: &'static str) -> Self {
        Header {
            islittle: NATIVE_LITTLE,
            // PUC's `initheader` starts with no alignment; `!n` opts in.
            maxalign: 1,
            who,
        }
    }
}

fn argerr(n: usize, who: &str, msg: &str) -> String {
    format!("bad argument #{n} to '{who}' ({msg})")
}

/// `luaL_checkinteger`: accepts integers, integral floats, and numeric strings.
fn check_integer<C>(lua: &Lua<C>, args: &[Value], i: usize, who: &str) -> Result<i64, String> {
    let v = arg(args, i);
    let coerced = match v {
        Value::Str(id) => parse_number(lua.strings.get(id)),
        _ => Some(v),
    };
    match coerced {
        Some(Value::Int(n)) => Ok(n),
        Some(Value::Float(f)) => float_to_exact_int(f).ok_or_else(|| {
            format!(
                "bad argument #{} to '{who}' (number has no integer representation)",
                i + 1
            )
        }),
        _ => Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn check_number<C>(lua: &Lua<C>, args: &[Value], i: usize, who: &str) -> Result<f64, String> {
    match arg(args, i) {
        Value::Int(n) => Ok(n as f64),
        Value::Float(f) => Ok(f),
        Value::Str(id) => match parse_number(lua.strings.get(id)) {
            Some(Value::Int(n)) => Ok(n as f64),
            Some(Value::Float(f)) => Ok(f),
            _ => Err(format!(
                "bad argument #{} to '{who}' (number expected, got string)",
                i + 1
            )),
        },
        v => Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn check_string<C>(lua: &Lua<C>, args: &[Value], i: usize, who: &str) -> Result<Vec<u8>, String> {
    match arg(args, i) {
        Value::Str(id) => Ok(lua.strings.get(id).to_vec()),
        v @ (Value::Int(_) | Value::Float(_)) => Ok(crate::value::fmt_number(v).into_bytes()),
        v => Err(format!(
            "bad argument #{} to '{who}' (string expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

/// `getnum`: read an optional decimal numeral, defaulting to `df`.
fn getnum(fmt: &[u8], pos: &mut usize, df: i64) -> i64 {
    if !fmt.get(*pos).is_some_and(u8::is_ascii_digit) {
        return df;
    }
    let mut a: i64 = 0;
    loop {
        a = a * 10 + (fmt[*pos] - b'0') as i64;
        *pos += 1;
        if !fmt.get(*pos).is_some_and(u8::is_ascii_digit) || a > (MAXSIZE as i64 - 9) / 10 {
            break;
        }
    }
    a
}

fn getnumlimit(fmt: &[u8], pos: &mut usize, df: i64) -> Result<usize, String> {
    let sz = getnum(fmt, pos, df);
    if sz > MAXINTSIZE as i64 || sz <= 0 {
        return Err(format!(
            "integral size ({sz}) out of limits [1,{MAXINTSIZE}]"
        ));
    }
    Ok(sz as usize)
}

/// `getoption`: classify the next format option, updating endianness/alignment.
fn getoption(h: &mut Header, fmt: &[u8], pos: &mut usize) -> Result<(KOption, usize), String> {
    let opt = *fmt
        .get(*pos)
        .ok_or_else(|| "invalid format option".to_string())?;
    *pos += 1;
    Ok(match opt {
        b'b' => (KOption::Kint, 1),
        b'B' => (KOption::Kuint, 1),
        b'h' => (KOption::Kint, std::mem::size_of::<c_short>()),
        b'H' => (KOption::Kuint, std::mem::size_of::<c_short>()),
        b'l' => (KOption::Kint, std::mem::size_of::<c_long>()),
        b'L' => (KOption::Kuint, std::mem::size_of::<c_long>()),
        b'j' => (KOption::Kint, SZINT),
        b'J' => (KOption::Kuint, SZINT),
        b'T' => (KOption::Kuint, std::mem::size_of::<usize>()),
        b'f' => (KOption::Kfloat, std::mem::size_of::<f32>()),
        b'n' => (KOption::Knumber, std::mem::size_of::<f64>()),
        b'd' => (KOption::Kdouble, std::mem::size_of::<f64>()),
        b'i' => (KOption::Kint, getnumlimit(fmt, pos, 4)?),
        b'I' => (KOption::Kuint, getnumlimit(fmt, pos, 4)?),
        b's' => (KOption::Kstring, getnumlimit(fmt, pos, 8)?),
        b'c' => {
            let sz = getnum(fmt, pos, -1);
            if sz == -1 {
                return Err("missing size for format option 'c'".to_string());
            }
            (KOption::Kchar, sz as usize)
        }
        b'z' => (KOption::Kzstr, 0),
        b'x' => (KOption::Kpadding, 1),
        b'X' => (KOption::Kpaddalign, 0),
        b' ' => (KOption::Knop, 0),
        b'<' => {
            h.islittle = true;
            (KOption::Knop, 0)
        }
        b'>' => {
            h.islittle = false;
            (KOption::Knop, 0)
        }
        b'=' => {
            h.islittle = NATIVE_LITTLE;
            (KOption::Knop, 0)
        }
        b'!' => {
            h.maxalign = getnumlimit(fmt, pos, MAXALIGN as i64)?;
            (KOption::Knop, 0)
        }
        _ => return Err(format!("invalid format option '{}'", opt as char)),
    })
}

/// `getdetails`: option kind, its own size, and the padding needed to align it.
fn getdetails(
    h: &mut Header,
    totalsize: usize,
    fmt: &[u8],
    pos: &mut usize,
) -> Result<(KOption, usize, usize), String> {
    let (opt, size) = getoption(h, fmt, pos)?;
    let mut align = size;
    if opt == KOption::Kpaddalign {
        // 'X' takes its alignment from the (consumed) following option
        if *pos >= fmt.len() {
            return Err(argerr(1, h.who, "invalid next option for option 'X'"));
        }
        let (nopt, nsize) = getoption(h, fmt, pos)?;
        align = nsize;
        if nopt == KOption::Kchar || align == 0 {
            return Err(argerr(1, h.who, "invalid next option for option 'X'"));
        }
    }
    let ntoalign = if align <= 1 || opt == KOption::Kchar {
        0
    } else {
        if align > h.maxalign {
            align = h.maxalign;
        }
        if align & (align - 1) != 0 {
            return Err(argerr(1, h.who, "format asks for alignment not power of 2"));
        }
        (align - (totalsize & (align - 1))) & (align - 1)
    };
    Ok((opt, size, ntoalign))
}

/// `packint`: write `n` as `size` bytes in the given endianness, sign-extending.
#[allow(clippy::needless_range_loop)]
fn packint(out: &mut Vec<u8>, mut n: u64, islittle: bool, size: usize, neg: bool) {
    let start = out.len();
    out.resize(start + size, 0);
    for i in 0..size {
        let byte = (n & 0xff) as u8;
        out[start + if islittle { i } else { size - 1 - i }] = byte;
        n >>= 8;
    }
    if neg && size > SZINT {
        for i in SZINT..size {
            out[start + if islittle { i } else { size - 1 - i }] = 0xff;
        }
    }
}

/// `unpackint`: read `size` bytes, sign-extending/overflow-checking like PUC.
#[allow(clippy::needless_range_loop)]
fn unpackint(
    data: &[u8],
    pos: usize,
    islittle: bool,
    size: usize,
    issigned: bool,
) -> Result<i64, String> {
    let mut res: u64 = 0;
    let limit = size.min(SZINT);
    for i in (0..limit).rev() {
        res <<= 8;
        res |= data[pos + if islittle { i } else { size - 1 - i }] as u64;
    }
    if size < SZINT {
        if issigned {
            let mask = 1u64 << (size * 8 - 1);
            res = (res ^ mask).wrapping_sub(mask);
        }
    } else if size > SZINT {
        let mask: u8 = if !issigned || (res as i64) >= 0 {
            0
        } else {
            0xff
        };
        for i in limit..size {
            if data[pos + if islittle { i } else { size - 1 - i }] != mask {
                return Err(format!("{size}-byte integer does not fit into Lua Integer"));
            }
        }
    }
    Ok(res as i64)
}

pub(crate) fn n_pack<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let fmt = check_string(lua, args, 0, "pack")?;
    let mut h = Header::new("pack");
    let mut out: Vec<u8> = Vec::new();
    let mut pos = 0usize;
    let mut argi = 1usize;
    while pos < fmt.len() {
        let (opt, size, ntoalign) = getdetails(&mut h, out.len(), &fmt, &mut pos)?;
        out.extend(std::iter::repeat_n(0u8, ntoalign));
        let mut consumes = true;
        match opt {
            KOption::Kint => {
                let n = check_integer(lua, args, argi, "pack")?;
                if size < SZINT {
                    let lim = 1i64 << (size * 8 - 1);
                    if !(-lim <= n && n < lim) {
                        return Err(argerr(argi + 1, "pack", "integer overflow"));
                    }
                }
                packint(&mut out, n as u64, h.islittle, size, n < 0);
            }
            KOption::Kuint => {
                let n = check_integer(lua, args, argi, "pack")?;
                if size < SZINT && (n as u64) >= (1u64 << (size * 8)) {
                    return Err(argerr(argi + 1, "pack", "unsigned overflow"));
                }
                packint(&mut out, n as u64, h.islittle, size, false);
            }
            KOption::Kfloat => {
                let f = check_number(lua, args, argi, "pack")? as f32;
                let bytes = if h.islittle {
                    f.to_le_bytes()
                } else {
                    f.to_be_bytes()
                };
                out.extend_from_slice(&bytes);
            }
            KOption::Knumber | KOption::Kdouble => {
                let f = check_number(lua, args, argi, "pack")?;
                let bytes = if h.islittle {
                    f.to_le_bytes()
                } else {
                    f.to_be_bytes()
                };
                out.extend_from_slice(&bytes);
            }
            KOption::Kchar => {
                let s = check_string(lua, args, argi, "pack")?;
                if s.len() > size {
                    return Err(argerr(argi + 1, "pack", "string longer than given size"));
                }
                out.extend_from_slice(&s);
                out.extend(std::iter::repeat_n(0u8, size - s.len()));
            }
            KOption::Kstring => {
                let s = check_string(lua, args, argi, "pack")?;
                if size < SZINT && s.len() as u64 >= (1u64 << (size * 8)) {
                    return Err(argerr(
                        argi + 1,
                        "pack",
                        "string length does not fit in given size",
                    ));
                }
                packint(&mut out, s.len() as u64, h.islittle, size, false);
                out.extend_from_slice(&s);
            }
            KOption::Kzstr => {
                let s = check_string(lua, args, argi, "pack")?;
                if s.contains(&0) {
                    return Err(argerr(argi + 1, "pack", "string contains zeros"));
                }
                out.extend_from_slice(&s);
                out.push(0);
            }
            KOption::Kpadding => out.push(0),
            KOption::Kpaddalign | KOption::Knop => consumes = false,
        }
        if consumes {
            argi += 1;
        }
    }
    Ok(vec![lua.new_string(&out)])
}

pub(crate) fn n_packsize<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let fmt = check_string(lua, args, 0, "packsize")?;
    let mut h = Header::new("packsize");
    let mut total = 0usize;
    let mut pos = 0usize;
    while pos < fmt.len() {
        let (opt, size, ntoalign) = getdetails(&mut h, total, &fmt, &mut pos)?;
        if opt == KOption::Kstring || opt == KOption::Kzstr {
            return Err(argerr(1, "packsize", "variable-length format"));
        }
        let size = size + ntoalign;
        if total > MAXSIZE - size {
            return Err(argerr(1, "packsize", "format result too large"));
        }
        total += size;
    }
    Ok(vec![Value::Int(total as i64)])
}

pub(crate) fn n_unpack<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let fmt = check_string(lua, args, 0, "unpack")?;
    let data = check_string(lua, args, 1, "unpack")?;
    let ld = data.len();
    let init = match arg(args, 2) {
        Value::Nil => 1,
        Value::Int(n) => n,
        Value::Float(f) => float_to_exact_int(f).ok_or_else(|| {
            "bad argument #3 to 'unpack' (number has no integer representation)".to_string()
        })?,
        v => {
            return Err(format!(
                "bad argument #3 to 'unpack' (number expected, got {})",
                v.type_name()
            ));
        }
    };
    // posrelatI: positive as-is, zero means 1, negative counts from the end,
    // and negatives at or before -ld clip to the start (PUC 5.4).
    // i128 avoids overflow for extreme (e.g. mininteger) positions.
    let ld_i = ld as i128;
    let start: i128 = if init > 0 {
        init as i128
    } else if init == 0 || (init as i128) < -ld_i {
        1
    } else {
        ld_i + init as i128 + 1
    };
    // PUC checks `pos <= ld` where `pos = start - 1`.
    if start > ld_i + 1 {
        return Err(argerr(3, "unpack", "initial position out of string"));
    }
    let mut pos = (start - 1) as usize;
    let mut h = Header::new("unpack");
    let mut results: Vec<Value> = Vec::new();
    let mut fmtpos = 0usize;
    while fmtpos < fmt.len() {
        let (opt, size, ntoalign) = getdetails(&mut h, pos, &fmt, &mut fmtpos)?;
        if ntoalign + size > ld - pos {
            return Err(argerr(2, "unpack", "data string too short"));
        }
        pos += ntoalign;
        match opt {
            KOption::Kint | KOption::Kuint => {
                results.push(Value::Int(unpackint(
                    &data,
                    pos,
                    h.islittle,
                    size,
                    opt == KOption::Kint,
                )?));
            }
            KOption::Kfloat => {
                let mut b = [0u8; 4];
                b.copy_from_slice(&data[pos..pos + 4]);
                let f = if h.islittle {
                    f32::from_le_bytes(b)
                } else {
                    f32::from_be_bytes(b)
                };
                results.push(Value::Float(f as f64));
            }
            KOption::Knumber | KOption::Kdouble => {
                let mut b = [0u8; 8];
                b.copy_from_slice(&data[pos..pos + 8]);
                let f = if h.islittle {
                    f64::from_le_bytes(b)
                } else {
                    f64::from_be_bytes(b)
                };
                results.push(Value::Float(f));
            }
            KOption::Kchar => {
                results.push(lua.new_string(&data[pos..pos + size]));
            }
            KOption::Kstring => {
                let len = unpackint(&data, pos, h.islittle, size, false)? as u64 as usize;
                if len > ld - pos - size {
                    return Err(argerr(2, "unpack", "data string too short"));
                }
                results.push(lua.new_string(&data[pos + size..pos + size + len]));
                pos += len;
            }
            KOption::Kzstr => {
                let len = data[pos..].iter().position(|&b| b == 0).unwrap_or(ld - pos);
                if pos + len >= ld {
                    return Err(argerr(2, "unpack", "unfinished string for format 'z'"));
                }
                results.push(lua.new_string(&data[pos..pos + len]));
                pos += len + 1;
            }
            KOption::Kpaddalign | KOption::Kpadding | KOption::Knop => {}
        }
        pos += size;
    }
    results.push(Value::Int(pos as i64 + 1));
    Ok(results)
}
