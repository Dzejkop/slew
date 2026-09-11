//! The `math` library. The PRNG is xoshiro256** with a fixed default seed:
//! scripts are deterministic by default, in keeping with strict execution
//! profiles. Embedders wanting real entropy can call `math.randomseed(n)`
//! with a seed of their choosing.

use crate::value::{float_to_exact_int, Value};
use crate::vm::Lua;

use super::{arg, set_field};

pub fn install(lua: &mut Lua) {
    let mt = lua.new_table();
    lua.set_global("math", mt);
    for (name, f) in [
        ("floor", n_floor as crate::vm::NativeFn),
        ("ceil", n_ceil),
        ("abs", n_abs),
        ("sqrt", n_sqrt),
        ("sin", n_sin),
        ("cos", n_cos),
        ("tan", n_tan),
        ("asin", n_asin),
        ("acos", n_acos),
        ("atan", n_atan),
        ("deg", n_deg),
        ("rad", n_rad),
        ("exp", n_exp),
        ("log", n_log),
        ("fmod", n_fmod),
        ("modf", n_modf),
        ("tointeger", n_tointeger),
        ("type", n_type),
        ("max", n_max),
        ("min", n_min),
        ("ult", n_ult),
        ("random", n_random),
        ("randomseed", n_randomseed),
    ] {
        let v = lua.add_native(name, f);
        set_field(lua, mt, name, v);
    }
    set_field(lua, mt, "pi", Value::Float(std::f64::consts::PI));
    set_field(lua, mt, "huge", Value::Float(f64::INFINITY));
    set_field(lua, mt, "maxinteger", Value::Int(i64::MAX));
    set_field(lua, mt, "mininteger", Value::Int(i64::MIN));
}

fn num(args: &[Value], i: usize, who: &str) -> Result<f64, String> {
    match arg(args, i) {
        Value::Int(n) => Ok(n as f64),
        Value::Float(f) => Ok(f),
        v => Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

fn int(args: &[Value], i: usize, who: &str) -> Result<i64, String> {
    match arg(args, i) {
        Value::Int(n) => Ok(n),
        Value::Float(f) => float_to_exact_int(f).ok_or_else(|| {
            format!("bad argument #{} to '{who}' (number has no integer representation)", i + 1)
        }),
        v => Err(format!(
            "bad argument #{} to '{who}' (number expected, got {})",
            i + 1,
            v.type_name()
        )),
    }
}

/// floor/ceil return integers when the result fits.
fn to_int_result(f: f64) -> Value {
    match float_to_exact_int(f) {
        Some(i) => Value::Int(i),
        None => Value::Float(f),
    }
}

fn n_floor(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        _ => to_int_result(num(args, 0, "floor")?.floor()),
    }])
}

fn n_ceil(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        _ => to_int_result(num(args, 0, "ceil")?.ceil()),
    }])
}

fn n_abs(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        Value::Int(i) => Value::Int(i.wrapping_abs()),
        _ => Value::Float(num(args, 0, "abs")?.abs()),
    }])
}

macro_rules! float_fn {
    ($name:ident, $who:literal, $method:ident) => {
        fn $name(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
            Ok(vec![Value::Float(num(args, 0, $who)?.$method())])
        }
    };
}

float_fn!(n_sqrt, "sqrt", sqrt);
float_fn!(n_sin, "sin", sin);
float_fn!(n_cos, "cos", cos);
float_fn!(n_tan, "tan", tan);
float_fn!(n_asin, "asin", asin);
float_fn!(n_acos, "acos", acos);
float_fn!(n_exp, "exp", exp);

fn n_atan(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let y = num(args, 0, "atan")?;
    let x = match arg(args, 1) {
        Value::Nil => 1.0,
        _ => num(args, 1, "atan")?,
    };
    Ok(vec![Value::Float(y.atan2(x))])
}

/// `math.deg`: radians → degrees (`x / (pi/180)`).
fn n_deg(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
    Ok(vec![Value::Float(num(args, 0, "deg")? / RADIANS_PER_DEGREE)])
}

/// `math.rad`: degrees → radians (`x * (pi/180)`).
fn n_rad(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
    Ok(vec![Value::Float(num(args, 0, "rad")? * RADIANS_PER_DEGREE)])
}

fn n_log(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let x = num(args, 0, "log")?;
    Ok(vec![Value::Float(match arg(args, 1) {
        Value::Nil => x.ln(),
        _ => {
            let base = num(args, 1, "log")?;
            if base == 2.0 {
                x.log2()
            } else if base == 10.0 {
                x.log10()
            } else {
                x.ln() / base.ln()
            }
        }
    })])
}

fn n_fmod(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    match (arg(args, 0), arg(args, 1)) {
        (Value::Int(a), Value::Int(b)) => {
            if b == 0 {
                return Err("bad argument #2 to 'fmod' (zero)".into());
            }
            Ok(vec![Value::Int(a.wrapping_rem(b))])
        }
        _ => {
            let a = num(args, 0, "fmod")?;
            let b = num(args, 1, "fmod")?;
            Ok(vec![Value::Float(a % b)])
        }
    }
}

fn n_modf(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let x = num(args, 0, "modf")?;
    let ip = x.trunc();
    Ok(vec![to_int_result(ip), Value::Float(if x.is_infinite() { 0.0 } else { x - ip })])
}

fn n_tointeger(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        Value::Float(f) => float_to_exact_int(f).map_or(Value::Nil, Value::Int),
        _ => Value::Nil,
    }])
}

fn n_type(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        Value::Int(_) => lua.new_string(b"integer"),
        Value::Float(_) => lua.new_string(b"float"),
        _ => Value::Nil,
    }])
}

fn minmax(args: &[Value], who: &str, want_max: bool) -> Result<Vec<Value>, String> {
    if args.is_empty() {
        return Err(format!("bad argument #1 to '{who}' (number expected, got no value)"));
    }
    let mut best = args[0];
    for (i, &v) in args.iter().enumerate() {
        num(args, i, who)?; // type check
        let cmp = match (v, best) {
            (Value::Int(a), Value::Int(b)) => a > b,
            (a, b) => to_f(a) > to_f(b),
        };
        if cmp == want_max {
            best = v;
        }
    }
    Ok(vec![best])
}

fn to_f(v: Value) -> f64 {
    match v {
        Value::Int(i) => i as f64,
        Value::Float(f) => f,
        _ => f64::NAN,
    }
}

fn n_max(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    minmax(args, "max", true)
}

fn n_min(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    minmax(args, "min", false)
}

fn n_ult(_: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let a = int(args, 0, "ult")? as u64;
    let b = int(args, 1, "ult")? as u64;
    Ok(vec![Value::Bool(a < b)])
}

// ---- deterministic PRNG (xoshiro256**) ----

fn n_random(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let r = lua.next_random();
    match (arg(args, 0), arg(args, 1)) {
        (Value::Nil, _) => {
            // float in [0, 1)
            Ok(vec![Value::Float((r >> 11) as f64 * (1.0 / (1u64 << 53) as f64))])
        }
        (_, Value::Nil) => {
            let m = int(args, 0, "random")?;
            if m < 1 {
                return Err("bad argument #1 to 'random' (interval is empty)".into());
            }
            Ok(vec![Value::Int(1 + (r % m as u64) as i64)])
        }
        _ => {
            let lo = int(args, 0, "random")?;
            let hi = int(args, 1, "random")?;
            if lo > hi {
                return Err("bad argument #2 to 'random' (interval is empty)".into());
            }
            let range = hi.wrapping_sub(lo) as u64;
            let off = if range == u64::MAX { r } else { r % (range + 1) };
            Ok(vec![Value::Int(lo.wrapping_add(off as i64))])
        }
    }
}

fn n_randomseed(lua: &mut Lua, args: &[Value]) -> Result<Vec<Value>, String> {
    let seed = match arg(args, 0) {
        Value::Nil => 0,
        Value::Int(i) => i as u64,
        Value::Float(f) => f.to_bits(),
        v => {
            return Err(format!(
                "bad argument #1 to 'randomseed' (number expected, got {})",
                v.type_name()
            ))
        }
    };
    lua.seed_random(seed);
    Ok(vec![])
}
