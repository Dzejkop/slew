//! The `math` library. The PRNG is PUC 5.4's xoshiro256** implementation,
//! ported byte-for-byte: `math.randomseed(n [, m])` seeds exactly as PUC's
//! `setseed` (state `{n, 0xff, m, 0}` plus 16 discarded draws), and
//! `math.random()`/`math.random(m)`/`math.random(m, n)`/`math.random(0)`
//! consume the generator identically. The default seed is fixed rather than
//! time-derived so scripts are deterministic; embedders reseed explicitly.

use crate::value::{Value, float_to_exact_int};
use crate::vm::Lua;

use super::{arg, set_field};

pub fn install<C>(lua: &mut Lua<C>) {
    let mt = lua.new_table();
    lua.set_global("math", mt);
    for (name, f) in [
        ("floor", n_floor as crate::vm::NativeFn<C>),
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

/// floor/ceil return integers when the result fits.
fn to_int_result(f: f64) -> Value {
    match float_to_exact_int(f) {
        Some(i) => Value::Int(i),
        None => Value::Float(f),
    }
}

fn n_floor<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        _ => to_int_result(num(args, 0, "floor")?.floor()),
    }])
}

fn n_ceil<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        _ => to_int_result(num(args, 0, "ceil")?.ceil()),
    }])
}

fn n_abs<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        Value::Int(i) => Value::Int(i.wrapping_abs()),
        _ => Value::Float(num(args, 0, "abs")?.abs()),
    }])
}

macro_rules! float_fn {
    ($name:ident, $who:literal, $method:ident) => {
        fn $name<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
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

fn n_atan<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let y = num(args, 0, "atan")?;
    let x = match arg(args, 1) {
        Value::Nil => 1.0,
        _ => num(args, 1, "atan")?,
    };
    Ok(vec![Value::Float(y.atan2(x))])
}

/// `math.deg`: radians → degrees (`x / (pi/180)`).
fn n_deg<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
    Ok(vec![Value::Float(
        num(args, 0, "deg")? / RADIANS_PER_DEGREE,
    )])
}

/// `math.rad`: degrees → radians (`x * (pi/180)`).
fn n_rad<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
    Ok(vec![Value::Float(
        num(args, 0, "rad")? * RADIANS_PER_DEGREE,
    )])
}

// `log2`/`log10` are selected only for the exact bases 2.0 and 10.0; an
// epsilon comparison would change behavior for other bases.
#[allow(clippy::float_cmp)]
fn n_log<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let x = num(args, 0, "log")?;
    Ok(vec![Value::Float(if arg(args, 1) == Value::Nil {
        x.ln()
    } else {
        let base = num(args, 1, "log")?;
        if base == 2.0 {
            x.log2()
        } else if base == 10.0 {
            x.log10()
        } else {
            x.ln() / base.ln()
        }
    })])
}

fn n_fmod<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    if let (Value::Int(a), Value::Int(b)) = (arg(args, 0), arg(args, 1)) {
        if b == 0 {
            return Err("bad argument #2 to 'fmod' (zero)".into());
        }
        Ok(vec![Value::Int(a.wrapping_rem(b))])
    } else {
        let a = num(args, 0, "fmod")?;
        let b = num(args, 1, "fmod")?;
        Ok(vec![Value::Float(a % b)])
    }
}

fn n_modf<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let x = num(args, 0, "modf")?;
    let ip = x.trunc();
    Ok(vec![
        to_int_result(ip),
        Value::Float(if x.is_infinite() { 0.0 } else { x - ip }),
    ])
}

fn n_tointeger<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        v @ Value::Int(_) => v,
        Value::Float(f) => float_to_exact_int(f).map_or(Value::Nil, Value::Int),
        // `lua_tointegerx` coerces numeric strings, including below-`i64::MIN`
        // decimals via the shared numeral parser.
        Value::Str(id) => match super::parse_number(lua.strings.get(id)) {
            Some(Value::Int(i)) => Value::Int(i),
            Some(Value::Float(f)) => float_to_exact_int(f).map_or(Value::Nil, Value::Int),
            _ => Value::Nil,
        },
        _ => Value::Nil,
    }])
}

fn n_type<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    Ok(vec![match arg(args, 0) {
        Value::Int(_) => lua.new_string(b"integer"),
        Value::Float(_) => lua.new_string(b"float"),
        _ => Value::Nil,
    }])
}

fn minmax(args: &[Value], who: &str, want_max: bool) -> Result<Vec<Value>, String> {
    if args.is_empty() {
        return Err(format!("bad argument #1 to '{who}' (value expected)"));
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

fn n_max<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    minmax(args, "math.max", true)
}

fn n_min<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    minmax(args, "math.min", false)
}

fn n_ult<C>(_: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    let a = int(args, 0, "ult")? as u64;
    let b = int(args, 1, "ult")? as u64;
    Ok(vec![Value::Bool(a < b)])
}

// ---- deterministic PRNG (PUC 5.4 xoshiro256**) ----

/// PUC's `I2d`: the top 53 bits of `x` scaled to `[0, 1)`.
fn i2d(x: u64) -> f64 {
    (x >> 11) as f64 * (1.0 / (1u64 << 53) as f64)
}

/// PUC's `project`: uniform projection of `ran` into `[0, n]`, drawing more
/// values from `lua` when the first lands outside.
fn project<C>(lua: &mut Lua<C>, mut ran: u64, n: u64) -> u64 {
    if n & n.wrapping_add(1) == 0 {
        return ran & n; // n + 1 is a power of two
    }
    let mut lim = n;
    lim |= lim >> 1;
    lim |= lim >> 2;
    lim |= lim >> 4;
    lim |= lim >> 8;
    lim |= lim >> 16;
    lim |= lim >> 32;
    loop {
        ran &= lim;
        if ran <= n {
            return ran;
        }
        ran = lua.next_random();
    }
}

fn n_random<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    // PUC draws the value before validating arguments, so failed calls still
    // advance the generator.
    let rv = lua.next_random();
    if args.len() > 2 {
        return Err("wrong number of arguments".into());
    }
    match (arg(args, 0), arg(args, 1)) {
        (Value::Nil, _) => Ok(vec![Value::Float(i2d(rv))]),
        (_, Value::Nil) => {
            let up = int(args, 0, "random")?;
            if up == 0 {
                // single 0: full random integer
                Ok(vec![Value::Int(rv as i64)])
            } else {
                if up < 1 {
                    return Err("bad argument #1 to 'random' (interval is empty)".into());
                }
                let p = project(lua, rv, (up as u64).wrapping_sub(1));
                Ok(vec![Value::Int(p.wrapping_add(1) as i64)])
            }
        }
        _ => {
            let lo = int(args, 0, "random")?;
            let up = int(args, 1, "random")?;
            if lo > up {
                return Err("bad argument #1 to 'random' (interval is empty)".into());
            }
            let p = project(lua, rv, (up as u64).wrapping_sub(lo as u64));
            Ok(vec![Value::Int(p.wrapping_add(lo as u64) as i64)])
        }
    }
}

fn n_randomseed<C>(lua: &mut Lua<C>, args: &[Value]) -> Result<Vec<Value>, String> {
    // `math.randomseed()` with no argument uses a time/address-derived seed in
    // PUC. slew has no ambient time authority, so it reuses its fixed default;
    // either way the returned pair fully reproduces the state.
    let (n1, n2) = if arg(args, 0) == Value::Nil {
        (0x536c_6577_5f5f_5f31_u64, 0u64)
    } else {
        let n1 = int(args, 0, "randomseed")? as u64;
        let n2 = match arg(args, 1) {
            Value::Nil => 0,
            _ => int(args, 1, "randomseed")? as u64,
        };
        (n1, n2)
    };
    lua.seed_random_pair(n1, n2);
    Ok(vec![Value::Int(n1 as i64), Value::Int(n2 as i64)])
}
