//! Values, interned strings, and tables.
//!
//! All GC objects live in arenas owned by the `Lua` state and are referenced
//! by index handles, which keeps `Value` `Copy` and makes a future mark-sweep
//! collector straightforward (no `Rc` cycles, no unsafe).

use std::collections::HashMap;
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct TableId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ClosId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct NativeId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UpvalId(pub u32);
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ThreadId(pub u32);

/// A Lua value. `PartialEq` is *raw* identity/bit equality (NaN ~= NaN, and
/// `Int(1) != Float(1.0)`); Lua `==` semantics live in the VM (`Lua::values_equal`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(StrId),
    Table(TableId),
    Closure(ClosId),
    Native(NativeId),
    Thread(ThreadId),
}

impl Value {
    pub fn truthy(self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    pub fn type_name(self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "boolean",
            Value::Int(_) | Value::Float(_) => "number",
            Value::Str(_) => "string",
            Value::Table(_) => "table",
            Value::Closure(_) | Value::Native(_) => "function",
            Value::Thread(_) => "thread",
        }
    }
}

/// String interner. Every live Lua string is interned, so `StrId` equality
/// is string equality and strings hash O(1) as table keys.
#[derive(Default)]
pub struct Strings {
    vec: Vec<Rc<[u8]>>,
    map: HashMap<Rc<[u8]>, StrId>,
}

impl Strings {
    pub fn intern(&mut self, s: &[u8]) -> StrId {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let rc: Rc<[u8]> = s.into();
        let id = StrId(self.vec.len() as u32);
        self.vec.push(rc.clone());
        self.map.insert(rc, id);
        id
    }

    /// Looks up an already-interned string without interning.
    pub fn lookup(&self, s: &[u8]) -> Option<StrId> {
        self.map.get(s).copied()
    }

    pub fn get(&self, id: StrId) -> &[u8] {
        &self.vec[id.0 as usize]
    }

    pub fn get_str_lossy(&self, id: StrId) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.get(id))
    }
}

/// Hashable table key. Floats with integral values are normalized to `Int`
/// before constructing one of these (so `t[1.0]` is `t[1]`, and `-0.0` is `0`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum HKey {
    Int(i64),
    /// Non-integral float, by bit pattern. Never NaN.
    Float(u64),
    Bool(bool),
    Str(StrId),
    Table(TableId),
    Closure(ClosId),
    Native(NativeId),
    Thread(ThreadId),
}

/// Converts a value to a table key per Lua 5.4 rules.
pub fn to_key(v: Value) -> Result<HKey, &'static str> {
    Ok(match v {
        Value::Nil => return Err("table index is nil"),
        Value::Bool(b) => HKey::Bool(b),
        Value::Int(i) => HKey::Int(i),
        Value::Float(f) => {
            if f.is_nan() {
                return Err("table index is NaN");
            }
            match float_to_exact_int(f) {
                Some(i) => HKey::Int(i),
                None => HKey::Float(f.to_bits()),
            }
        }
        Value::Str(s) => HKey::Str(s),
        Value::Table(t) => HKey::Table(t),
        Value::Closure(c) => HKey::Closure(c),
        Value::Native(n) => HKey::Native(n),
        Value::Thread(t) => HKey::Thread(t),
    })
}

/// `Some(i)` iff `f` represents exactly the integer `i` (in i64 range).
pub fn float_to_exact_int(f: f64) -> Option<i64> {
    if f.fract() == 0.0 && (-9.223372036854776e18..9.223372036854776e18).contains(&f) {
        Some(f as i64)
    } else {
        None
    }
}

#[derive(Default)]
pub struct Table {
    /// Dense array part for keys `1..=array.len()` (may contain trailing nils).
    array: Vec<Value>,
    hash: HashMap<HKey, Value>,
    pub metatable: Option<TableId>,
}

impl Table {
    pub fn get(&self, key: Value) -> Value {
        let Ok(k) = to_key(key) else { return Value::Nil };
        self.get_key(k)
    }

    fn get_key(&self, k: HKey) -> Value {
        if let HKey::Int(i) = k
            && i >= 1 && (i as usize) <= self.array.len() {
                return self.array[i as usize - 1];
            }
        self.hash.get(&k).copied().unwrap_or(Value::Nil)
    }

    pub fn set(&mut self, key: Value, value: Value) -> Result<(), &'static str> {
        let k = to_key(key)?;
        if let HKey::Int(i) = k {
            if i >= 1 && (i as usize) <= self.array.len() {
                self.array[i as usize - 1] = value;
                return Ok(());
            }
            if i as usize == self.array.len() + 1 && value != Value::Nil {
                self.array.push(value);
                // migrate any subsequent keys from the hash part
                loop {
                    let next = HKey::Int(self.array.len() as i64 + 1);
                    match self.hash.remove(&next) {
                        Some(v) => self.array.push(v),
                        None => break,
                    }
                }
                return Ok(());
            }
        }
        if value == Value::Nil {
            self.hash.remove(&k);
        } else {
            self.hash.insert(k, value);
        }
        Ok(())
    }

    /// The `#` operator: a border of the table.
    pub fn length(&self) -> i64 {
        let n = self.array.len();
        if n > 0 && self.array[n - 1] == Value::Nil {
            // binary search for a border within the array part
            let (mut i, mut j) = (0usize, n);
            while j - i > 1 {
                let m = (i + j) / 2;
                if self.array[m - 1] == Value::Nil {
                    j = m;
                } else {
                    i = m;
                }
            }
            return i as i64;
        }
        if self.hash.is_empty() {
            return n as i64;
        }
        let mut i = n as i64;
        while self.get_key(HKey::Int(i + 1)) != Value::Nil {
            i += 1;
        }
        i
    }

    /// Iteration support for `next`: a stable snapshot order is array part
    /// then hash part. O(n) per call; fine until we move to an ordered map.
    pub fn next_after(&self, key: Option<HKey>) -> Option<(Value, Value)> {
        let array_iter = (1..=self.array.len() as i64).map(HKey::Int);
        let mut all = array_iter.chain(self.hash.keys().copied());
        if let Some(prev) = key {
            // skip until just past `prev`
            let mut found = false;
            for k in all.by_ref() {
                if k == prev {
                    found = true;
                    break;
                }
            }
            if !found {
                return None;
            }
        }
        for k in all {
            let v = self.get_key(k);
            if v != Value::Nil {
                return Some((key_to_value(k), v));
            }
        }
        None
    }
}

pub fn key_to_value(k: HKey) -> Value {
    match k {
        HKey::Int(i) => Value::Int(i),
        HKey::Float(b) => Value::Float(f64::from_bits(b)),
        HKey::Bool(b) => Value::Bool(b),
        HKey::Str(s) => Value::Str(s),
        HKey::Table(t) => Value::Table(t),
        HKey::Closure(c) => Value::Closure(c),
        HKey::Native(n) => Value::Native(n),
        HKey::Thread(t) => Value::Thread(t),
    }
}

/// Formats a float like Lua 5.4's `%.14g`, with the trailing `.0` added when
/// the result would otherwise look like an integer.
pub fn fmt_float(x: f64) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if x == 0.0 {
        return "0.0".into();
    }
    let sci = format!("{:.13e}", x);
    let epos = sci.find('e').unwrap();
    let exp: i32 = sci[epos + 1..].parse().unwrap();
    let mut s = if (-4..14).contains(&exp) {
        let prec = (13 - exp).max(0) as usize;
        let mut s = format!("{x:.prec$}");
        if s.contains('.') {
            while s.ends_with('0') {
                s.pop();
            }
            if s.ends_with('.') {
                s.pop();
            }
        }
        s
    } else {
        let mut m = sci[..epos].to_string();
        if m.contains('.') {
            while m.ends_with('0') {
                m.pop();
            }
            if m.ends_with('.') {
                m.pop();
            }
        }
        format!("{m}e{}{:02}", if exp >= 0 { "+" } else { "-" }, exp.abs())
    };
    if !s.contains(['.', 'e']) {
        s.push_str(".0");
    }
    s
}

/// Formats a number for `tostring`/concatenation.
pub fn fmt_number(v: Value) -> String {
    match v {
        Value::Int(i) => i.to_string(),
        Value::Float(f) => fmt_float(f),
        _ => unreachable!("fmt_number on non-number"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::approx_constant)]
    fn float_formatting() {
        assert_eq!(fmt_float(1.0), "1.0");
        assert_eq!(fmt_float(-1.0), "-1.0");
        assert_eq!(fmt_float(0.5), "0.5");
        assert_eq!(fmt_float(3.1416), "3.1416");
        assert_eq!(fmt_float(1.0 / 3.0), "0.33333333333333");
        assert_eq!(fmt_float(1e15), "1e+15");
        assert_eq!(fmt_float(1e13), "10000000000000.0");
        assert_eq!(fmt_float(1e-5), "1e-05");
        assert_eq!(fmt_float(f64::INFINITY), "inf");
        assert_eq!(fmt_float(2.5e-3), "0.0025");
    }

    #[test]
    fn table_basics() {
        let mut t = Table::default();
        t.set(Value::Int(1), Value::Int(10)).unwrap();
        t.set(Value::Int(2), Value::Int(20)).unwrap();
        t.set(Value::Float(3.0), Value::Int(30)).unwrap(); // normalizes to Int(3)
        assert_eq!(t.get(Value::Int(3)), Value::Int(30));
        assert_eq!(t.get(Value::Float(1.0)), Value::Int(10));
        assert_eq!(t.length(), 3);
        assert!(t.set(Value::Nil, Value::Int(1)).is_err());
        assert!(t.set(Value::Float(f64::NAN), Value::Int(1)).is_err());
    }

    #[test]
    fn table_hash_to_array_migration() {
        let mut t = Table::default();
        t.set(Value::Int(2), Value::Int(2)).unwrap();
        t.set(Value::Int(3), Value::Int(3)).unwrap();
        assert_eq!(t.length(), 0);
        t.set(Value::Int(1), Value::Int(1)).unwrap();
        assert_eq!(t.length(), 3);
    }

    #[test]
    fn table_border_with_holes() {
        let mut t = Table::default();
        for i in 1..=5 {
            t.set(Value::Int(i), Value::Int(i)).unwrap();
        }
        t.set(Value::Int(5), Value::Nil).unwrap();
        assert_eq!(t.length(), 4);
    }
}
