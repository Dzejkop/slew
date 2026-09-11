//! Values, interned strings, and tables.
//!
//! All GC objects live in arenas owned by the `Lua` state and are referenced
//! by index handles, which keeps `Value` `Copy` and makes a future mark-sweep
//! collector straightforward (no `Rc` cycles, no unsafe).

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::rc::Rc;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct StrId(pub u32);

/// PUC's `LUAI_MAXSHORTLEN`: strings up to this length are *short* and always
/// interned; longer strings are *long* and (when created at runtime) get a
/// fresh object identity.
pub const MAXSHORTLEN: usize = 40;

/// A reference to a Lua string.
///
/// `obj` is the object identity (distinct `%p` for distinct runtime long
/// strings), `content` is a canonical object handle with the same bytes used
/// for equality and table keys. For interned strings the two coincide.
#[derive(Clone, Copy, Debug)]
pub struct StrRef {
    pub obj: StrId,
    /// Canonical handle whose `StrId` uniquely identifies the byte content.
    pub content: StrId,
}

impl StrRef {
    /// A reference to a string already known to be canonical (interned), so its
    /// object handle also identifies its content.
    pub const fn interned(id: StrId) -> Self {
        StrRef {
            obj: id,
            content: id,
        }
    }
}

impl PartialEq for StrRef {
    fn eq(&self, other: &Self) -> bool {
        self.content == other.content
    }
}

impl Eq for StrRef {}

impl Hash for StrRef {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.content.hash(state);
    }
}

impl From<StrRef> for StrId {
    fn from(r: StrRef) -> StrId {
        r.obj
    }
}
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
/// Handle into the userdata arena. Userdata carries an optional metatable and
/// a host-object payload (see [`crate::host::Userdata`]).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct UserdataId(pub u32);

/// A Lua value. `PartialEq` is *raw* identity/bit equality (NaN ~= NaN, and
/// `Int(1) != Float(1.0)`), except strings which compare by content (see
/// [`StrRef`]); Lua `==` semantics live in the VM (`Lua::values_equal`).
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(StrRef),
    Table(TableId),
    Closure(ClosId),
    Native(NativeId),
    Thread(ThreadId),
    Userdata(UserdataId),
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
            Value::Userdata(_) => "userdata",
        }
    }
}

/// String store. Short strings (length <= [`MAXSHORTLEN`]) and compile-time
/// literals are *interned*: equal content is one object. Long strings created
/// at runtime are not deduplicated — each gets its own object — but every
/// object also records a canonical *content* handle so equality and table keys
/// stay content-based (see [`StrRef`]).
///
/// `map` links a byte content to the canonical live object holding it. An
/// object's content handle is always a live object with the same bytes, and
/// the collector keeps it alive as long as any value references that content.
///
/// Strings referenced from compiled `Proto` constants are interned as
/// *fixed*: protos live outside the GC heap (host-held `Chunk`s, `Rc`s in
/// closures), so their strings are never swept. Everything created at
/// runtime is collectable.
#[derive(Default)]
pub struct Strings {
    vec: Vec<Option<Rc<[u8]>>>,
    fixed: Vec<bool>,
    /// Content bytes -> canonical object handle holding them.
    map: HashMap<Rc<[u8]>, StrId>,
    free: Vec<u32>,
    /// Total bytes of live string data.
    bytes: usize,
}

impl Strings {
    /// Interns `s`, returning a canonical reference (deduplicated by content).
    pub fn intern(&mut self, s: &[u8]) -> StrRef {
        let id = self.intern_canonical(s, false);
        StrRef::interned(id)
    }

    /// Interns a string that is never garbage collected.
    pub fn intern_fixed(&mut self, s: &[u8]) -> StrRef {
        let id = self.intern_canonical(s, true);
        StrRef::interned(id)
    }

    /// Creates a string produced at runtime. Short strings are interned exactly
    /// like literals; long strings get a fresh object identity while sharing a
    /// canonical content handle, matching PUC's short/long string model.
    pub fn new_string(&mut self, s: &[u8]) -> StrRef {
        if s.len() <= MAXSHORTLEN {
            return self.intern(s);
        }
        let rc: Rc<[u8]> = s.into();
        let obj = self.alloc(rc.clone(), false);
        let content = match self.map.get(&rc) {
            Some(&c) => c,
            None => {
                self.map.insert(rc, obj);
                obj
            }
        };
        StrRef { obj, content }
    }

    fn intern_canonical(&mut self, s: &[u8], fixed: bool) -> StrId {
        if let Some(&id) = self.map.get(s) {
            if fixed {
                self.fixed[id.0 as usize] = true;
            }
            return id;
        }
        let rc: Rc<[u8]> = s.into();
        let id = self.alloc(rc.clone(), fixed);
        self.map.insert(rc, id);
        id
    }

    /// Allocates a fresh object slot holding `rc`.
    fn alloc(&mut self, rc: Rc<[u8]>, fixed: bool) -> StrId {
        self.bytes += rc.len();
        match self.free.pop() {
            Some(slot) => {
                self.vec[slot as usize] = Some(rc);
                self.fixed[slot as usize] = fixed;
                StrId(slot)
            }
            None => {
                self.vec.push(Some(rc));
                self.fixed.push(fixed);
                StrId(self.vec.len() as u32 - 1)
            }
        }
    }

    /// Looks up the canonical reference for `s` without interning.
    pub fn lookup(&self, s: &[u8]) -> Option<StrRef> {
        self.map.get(s).copied().map(StrRef::interned)
    }

    /// Returns the bytes of a string, given either an object or content handle.
    pub fn get(&self, id: impl Into<StrId>) -> &[u8] {
        let id = id.into();
        self.vec[id.0 as usize].as_deref().expect("stale StrId")
    }

    pub fn get_str_lossy(&self, id: impl Into<StrId>) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(self.get(id))
    }

    pub fn len(&self) -> usize {
        self.vec.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }

    pub fn bytes(&self) -> usize {
        self.bytes
    }

    /// Number of live (unswept) string objects.
    pub fn live_count(&self) -> usize {
        self.vec.iter().filter(|slot| slot.is_some()).count()
    }

    /// Sweeps unmarked, non-fixed strings. `marked` is indexed by `StrId`.
    pub(crate) fn sweep(&mut self, marked: &[bool]) -> usize {
        let mut freed = 0;
        for i in 0..self.vec.len() {
            if self.fixed[i] || marked.get(i).copied().unwrap_or(false) {
                continue;
            }
            let Some(rc) = self.vec[i].take() else {
                continue;
            };
            self.bytes -= rc.len();
            // Drop the content mapping only if this object is the canonical
            // representative; a non-canonical long string shares its content
            // with a (still live) canonical object.
            if self.map.get(&rc) == Some(&StrId(i as u32)) {
                self.map.remove(&rc);
            }
            self.free.push(i as u32);
            freed += 1;
        }
        freed
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
    Str(StrRef),
    Table(TableId),
    Closure(ClosId),
    Native(NativeId),
    Thread(ThreadId),
    Userdata(UserdataId),
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
        Value::Userdata(u) => HKey::Userdata(u),
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

/// Returned by [`Table::next_after`] when the iteration key is not present in
/// the table; the base library renders this as Lua's "invalid key to 'next'".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidKey;

#[derive(Default)]
pub struct Table {
    /// Dense array part for keys `1..=array.len()` (may contain trailing nils).
    array: Vec<Value>,
    hash: HashMap<HKey, Value>,
    /// Insertion order of every key ever placed in the hash part. Removed keys
    /// stay here as tombstones so an in-progress `next` traversal can still
    /// locate a key that was set to nil mid-iteration (PUC keeps its dead
    /// keys around for exactly this reason). Never mutated during iteration,
    /// so the cursor survives deletion and collection of the current key.
    order: Vec<HKey>,
    /// Position of a key in `order`, also serving as "was this key ever
    /// inserted" so re-inserting a removed key does not add a duplicate.
    order_pos: HashMap<HKey, usize>,
    pub metatable: Option<TableId>,
    /// Set once the collector has selected this table for a `__gc` run, so a
    /// resurrected object is never finalized twice.
    pub finalized: bool,
}

impl Table {
    pub fn get(&self, key: Value) -> Value {
        let Ok(k) = to_key(key) else {
            return Value::Nil;
        };
        self.get_key(k)
    }

    fn get_key(&self, k: HKey) -> Value {
        if let HKey::Int(i) = k
            && i >= 1
            && (i as usize) <= self.array.len()
        {
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
            if !self.order_pos.contains_key(&k) {
                self.order_pos.insert(k, self.order.len());
                self.order.push(k);
            }
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

    /// Visits every value reachable from this table (array values, hash
    /// keys and values) — for the garbage collector.
    pub(crate) fn trace(&self, mut f: impl FnMut(Value)) {
        for &v in &self.array {
            f(v);
        }
        for (&k, &v) in &self.hash {
            f(key_to_value(k));
            f(v);
        }
    }

    /// Snapshot of every live (non-nil) entry as Lua key/value pairs. Used by
    /// the collector to clear dead weak entries; O(n) and allocation-heavy,
    /// which is fine because it only runs during a collection.
    pub(crate) fn entries(&self) -> Vec<(Value, Value)> {
        let mut out = Vec::with_capacity(self.array.len() + self.hash.len());
        for (i, &v) in self.array.iter().enumerate() {
            if v != Value::Nil {
                out.push((Value::Int(i as i64 + 1), v));
            }
        }
        for (&k, &v) in &self.hash {
            if v != Value::Nil {
                out.push((key_to_value(k), v));
            }
        }
        out
    }

    /// Removes an entry by key (a table key produced by [`Table::entries`]).
    pub(crate) fn remove(&mut self, key: Value) {
        let _ = self.set(key, Value::Nil);
    }

    /// Rough heap footprint in bytes, for memory budgeting.
    pub fn mem_estimate(&self) -> usize {
        64 + self.array.capacity() * 16
            + self.hash.capacity() * 48
            + self.order.capacity() * 16
            + self.order_pos.capacity() * 48
    }

    /// Iteration support for `next`: a stable order is array part, then hash
    /// part in insertion order. O(n) per call; fine until we move to an
    /// ordered map.
    ///
    /// Returns `Err(InvalidKey)` when `key` was never in the table, which the
    /// base library turns into an "invalid key to 'next'" error. A key that
    /// was removed is still locatable (its `order` slot is a tombstone), so
    /// deleting the current key mid-iteration is safe.
    pub fn next_after(&self, key: Option<HKey>) -> Result<Option<(Value, Value)>, InvalidKey> {
        let mut all = self.iter_keys();
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
                return Err(InvalidKey);
            }
        }
        for k in all {
            let v = self.get_key(k);
            if v != Value::Nil {
                return Ok(Some((key_to_value(k), v)));
            }
        }
        Ok(None)
    }

    /// The `next` traversal order: array indices `1..=len`, then hash keys in
    /// insertion order. Integer keys that have migrated into the array part
    /// are skipped in the hash phase so a key is never visited twice.
    fn iter_keys(&self) -> impl Iterator<Item = HKey> + '_ {
        let array_len = self.array.len() as i64;
        (1..=array_len).map(HKey::Int).chain(
            self.order
                .iter()
                .copied()
                .filter(move |k| !matches!(k, HKey::Int(i) if *i >= 1 && *i <= array_len)),
        )
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
        HKey::Userdata(u) => Value::Userdata(u),
    }
}

/// Formats a finite, non-zero float like C's `%.<prec>g`.
pub fn fmt_g(x: f64, prec: usize) -> String {
    if x.is_nan() {
        return "nan".into();
    }
    if x.is_infinite() {
        return if x < 0.0 { "-inf".into() } else { "inf".into() };
    }
    if x == 0.0 {
        return "0".into();
    }
    let prec = prec.max(1) as i32;
    let sci = format!("{:.*e}", prec as usize - 1, x);
    let epos = sci.find('e').unwrap();
    let exp: i32 = sci[epos + 1..].parse().unwrap();
    if exp >= -4 && exp < prec {
        let p = (prec - 1 - exp).max(0) as usize;
        let mut s = format!("{x:.p$}");
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
    }
}

/// Formats a float like Lua 5.4's `%.14g`, with the trailing `.0` added when
/// the result would otherwise look like an integer.
pub fn fmt_float(x: f64) -> String {
    if x == 0.0 {
        return "0.0".into();
    }
    let mut s = fmt_g(x, 14);
    if !s.contains(['.', 'e', 'n', 'i']) {
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
