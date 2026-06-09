//! The stackless VM and the public embedding API.
//!
//! Lua call frames live in `Thread::frames` (a Vec, never the Rust call
//! stack), so the dispatch loop can stop after any instruction and resume
//! later: that is what makes executions suspendable at arbitrary points.

use crate::bytecode::{ArithOp, CmpOp, Instr, Proto, UnaryOp, UpvalDesc};
use crate::compiler::{compile, CompileError};
use crate::parser::{parse, ParseError};
use crate::value::{
    fmt_number, float_to_exact_int, ClosId, NativeId, Strings, Table, TableId, ThreadId,
    UpvalId, Value,
};
use std::fmt;
use std::rc::Rc;

/// Default cap on call-frame depth; a deliberately bounded execution profile
/// knob (recursion consumes heap, not the host stack).
const MAX_CALL_DEPTH: usize = 10_000;

#[derive(Debug)]
pub enum Error {
    Parse(ParseError),
    Compile(CompileError),
    Runtime(RuntimeError),
}

#[derive(Debug, Clone)]
pub struct RuntimeError {
    pub message: String,
    pub line: u32,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Parse(e) => write!(f, "{e}"),
            Error::Compile(e) => write!(f, "{e}"),
            Error::Runtime(e) => write!(f, "runtime error at line {}: {}", e.line, e.message),
        }
    }
}

impl std::error::Error for Error {}

impl From<ParseError> for Error {
    fn from(e: ParseError) -> Self {
        Error::Parse(e)
    }
}

impl From<CompileError> for Error {
    fn from(e: CompileError) -> Self {
        Error::Compile(e)
    }
}

pub type NativeFn = fn(&mut Lua, &[Value]) -> Result<Vec<Value>, String>;

pub(crate) struct Native {
    pub name: String,
    pub f: NativeFn,
}

pub(crate) struct LuaClosure {
    pub proto: Rc<Proto>,
    pub upvals: Vec<UpvalId>,
}

pub(crate) enum Upval {
    /// Live in a thread's register stack.
    Open(ThreadId, usize),
    Closed(Value),
}

struct Frame {
    closure: ClosId,
    proto: Rc<Proto>,
    pc: usize,
    base: usize,
    /// Absolute stack slot where results go (the callee's function slot).
    ret_to: usize,
    /// Results expected by the caller: count+1, or 0 for multret.
    nres: u8,
    varargs: Vec<Value>,
}

#[derive(Default)]
pub(crate) struct Thread {
    stack: Vec<Value>,
    frames: Vec<Frame>,
    /// Open upvalues, sorted by stack index.
    open_upvals: Vec<(usize, UpvalId)>,
    /// Top of the last multret sequence (absolute).
    top: usize,
}

/// A compiled script, reusable across executions.
#[derive(Clone)]
pub struct Chunk {
    pub(crate) proto: Rc<Proto>,
}

impl fmt::Debug for Chunk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Chunk({})", self.proto.name)
    }
}

/// Result of driving an execution by one fuel budget.
#[derive(Debug, PartialEq)]
pub enum Step {
    /// The script finished and returned these values.
    Done(Vec<Value>),
    /// Fuel ran out; call `step` again to continue.
    Pending,
}

/// A suspendable run of a chunk. Created by [`Lua::execute`]; all VM state
/// lives in the `Lua`, this is a handle plus fuel-debt bookkeeping.
pub struct Execution {
    thread: ThreadId,
    /// Fuel overdrawn by the last step (surcharges can overshoot), repaid
    /// from the next budget.
    debt: i64,
    finished: bool,
}

pub struct Lua {
    pub(crate) strings: Strings,
    pub(crate) tables: Vec<Table>,
    pub(crate) closures: Vec<LuaClosure>,
    pub(crate) natives: Vec<Native>,
    pub(crate) upvals: Vec<Upval>,
    pub(crate) threads: Vec<Thread>,
    pub(crate) globals: TableId,
    pub(crate) builtin_next: Value,
    pub(crate) builtin_ipairs_iter: Value,
}

impl Default for Lua {
    fn default() -> Self {
        Self::new()
    }
}

impl Lua {
    pub fn new() -> Self {
        let mut lua = Lua {
            strings: Strings::default(),
            tables: vec![Table::default()],
            closures: Vec::new(),
            natives: Vec::new(),
            upvals: Vec::new(),
            threads: Vec::new(),
            globals: TableId(0),
            builtin_next: Value::Nil,
            builtin_ipairs_iter: Value::Nil,
        };
        crate::stdlib::install(&mut lua);
        lua
    }

    /// Parses and compiles a script. No code runs.
    pub fn load(&mut self, src: impl AsRef<[u8]>) -> Result<Chunk, Error> {
        let block = parse(src.as_ref())?;
        let proto = compile(&block, &mut self.strings, "chunk")?;
        Ok(Chunk { proto })
    }

    /// Instantiates a chunk as a suspended execution. Nothing runs until
    /// [`Execution::step`] is called.
    pub fn execute(&mut self, chunk: &Chunk) -> Execution {
        let env = self.new_upval(Upval::Closed(Value::Table(self.globals)));
        let cid = ClosId(self.closures.len() as u32);
        self.closures.push(LuaClosure { proto: chunk.proto.clone(), upvals: vec![env] });
        let mut th = Thread::default();
        th.stack.resize(chunk.proto.max_regs as usize, Value::Nil);
        th.frames.push(Frame {
            closure: cid,
            proto: chunk.proto.clone(),
            pc: 0,
            base: 0,
            ret_to: 0,
            nres: 0,
            varargs: Vec::new(),
        });
        let tid = ThreadId(self.threads.len() as u32);
        self.threads.push(th);
        Execution { thread: tid, debt: 0, finished: false }
    }

    /// Registers a native function as a global.
    pub fn register_native(&mut self, name: &str, f: NativeFn) -> Value {
        let v = self.add_native(name, f);
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
        v
    }

    /// Adds a native function without binding it to a global.
    pub fn add_native(&mut self, name: &str, f: NativeFn) -> Value {
        let id = NativeId(self.natives.len() as u32);
        self.natives.push(Native { name: name.into(), f });
        Value::Native(id)
    }

    pub fn new_string(&mut self, s: &[u8]) -> Value {
        Value::Str(self.strings.intern(s))
    }

    pub fn new_table(&mut self) -> Value {
        let id = TableId(self.tables.len() as u32);
        self.tables.push(Table::default());
        Value::Table(id)
    }

    fn new_upval(&mut self, u: Upval) -> UpvalId {
        let id = UpvalId(self.upvals.len() as u32);
        self.upvals.push(u);
        id
    }

    pub fn get_global(&self, name: &str) -> Value {
        match self.strings.lookup(name.as_bytes()) {
            Some(id) => self.tables[self.globals.0 as usize].get(Value::Str(id)),
            None => Value::Nil, // a name never interned can't be a set global
        }
    }

    pub fn set_global(&mut self, name: &str, v: Value) {
        let k = self.new_string(name.as_bytes());
        self.tables[self.globals.0 as usize].set(k, v).unwrap();
    }

    pub fn table_get(&self, t: Value, k: Value) -> Value {
        match t {
            Value::Table(id) => self.tables[id.0 as usize].get(k),
            _ => Value::Nil,
        }
    }

    pub fn str_bytes(&self, v: Value) -> Option<&[u8]> {
        match v {
            Value::Str(id) => Some(self.strings.get(id)),
            _ => None,
        }
    }

    /// Human-readable rendering of a value (like `tostring`, lossy for
    /// non-UTF-8 strings).
    pub fn display_value(&self, v: Value) -> String {
        match v {
            Value::Nil => "nil".into(),
            Value::Bool(b) => b.to_string(),
            Value::Int(_) | Value::Float(_) => fmt_number(v),
            Value::Str(id) => self.strings.get_str_lossy(id).into_owned(),
            Value::Table(t) => format!("table: 0x{:08x}", t.0),
            Value::Closure(c) => format!("function: 0x{:08x}", c.0),
            Value::Native(n) => format!("function: builtin: {}", self.natives[n.0 as usize].name),
        }
    }

    // ---- dispatch ----

    fn run(
        &mut self,
        tid: ThreadId,
        fuel: &mut i64,
    ) -> Result<Option<Vec<Value>>, RuntimeError> {
        let mut th = std::mem::take(&mut self.threads[tid.0 as usize]);
        let r = self.dispatch(tid, &mut th, fuel);
        if r.is_err() {
            th.frames.clear();
            th.stack.clear();
        }
        self.threads[tid.0 as usize] = th;
        r
    }

    fn dispatch(
        &mut self,
        tid: ThreadId,
        th: &mut Thread,
        fuel: &mut i64,
    ) -> Result<Option<Vec<Value>>, RuntimeError> {
        loop {
            if *fuel <= 0 {
                return Ok(None);
            }
            *fuel -= 1;
            let (instr, base) = {
                let f = th.frames.last_mut().unwrap();
                let i = f.proto.code[f.pc];
                f.pc += 1;
                (i, f.base)
            };
            match instr {
                Instr::LoadK { dst, k } => {
                    th.stack[base + dst as usize] = kval(th, k);
                }
                Instr::LoadNil { dst, n } => {
                    for i in 0..n as usize {
                        th.stack[base + dst as usize + i] = Value::Nil;
                    }
                }
                Instr::LoadBool { dst, b } => {
                    th.stack[base + dst as usize] = Value::Bool(b);
                }
                Instr::Move { dst, src } => {
                    th.stack[base + dst as usize] = th.stack[base + src as usize];
                }
                Instr::GetUpval { dst, up } => {
                    let id = self.frame_upval(th, up);
                    th.stack[base + dst as usize] = self.read_upval(tid, th, id);
                }
                Instr::SetUpval { up, src } => {
                    let id = self.frame_upval(th, up);
                    let v = th.stack[base + src as usize];
                    self.write_upval(tid, th, id, v);
                }
                Instr::GetIndex { dst, obj, key } => {
                    let o = th.stack[base + obj as usize];
                    let k = th.stack[base + key as usize];
                    th.stack[base + dst as usize] = self.index_value(th, o, k)?;
                }
                Instr::GetField { dst, obj, k } => {
                    let o = th.stack[base + obj as usize];
                    let key = kval(th, k);
                    th.stack[base + dst as usize] = self.index_value(th, o, key)?;
                }
                Instr::SetIndex { obj, key, src } => {
                    let o = th.stack[base + obj as usize];
                    let k = th.stack[base + key as usize];
                    let v = th.stack[base + src as usize];
                    self.setindex_value(th, o, k, v)?;
                }
                Instr::SetField { obj, k, src } => {
                    let o = th.stack[base + obj as usize];
                    let key = kval(th, k);
                    let v = th.stack[base + src as usize];
                    self.setindex_value(th, o, key, v)?;
                }
                Instr::NewTable { dst } => {
                    *fuel -= 2;
                    th.stack[base + dst as usize] = self.new_table();
                }
                Instr::SetList { obj, base: b, n, start } => {
                    let t = match th.stack[base + obj as usize] {
                        Value::Table(t) => t,
                        _ => unreachable!("SetList on non-table"),
                    };
                    let first = base + b as usize;
                    let count = if n == 0 {
                        th.top.saturating_sub(first)
                    } else {
                        n as usize
                    };
                    *fuel -= count as i64;
                    for i in 0..count {
                        let v = th.stack[first + i];
                        self.tables[t.0 as usize]
                            .set(Value::Int(start as i64 + i as i64), v)
                            .map_err(|m| self.rt_err(th, m.to_string()))?;
                    }
                }
                Instr::Arith { op, dst, lhs, rhs } => {
                    let a = th.stack[base + lhs as usize];
                    let b = th.stack[base + rhs as usize];
                    th.stack[base + dst as usize] =
                        arith(op, a, b).map_err(|m| self.rt_err(th, m))?;
                }
                Instr::Unary { op, dst, src } => {
                    let v = th.stack[base + src as usize];
                    th.stack[base + dst as usize] = self.unary(th, op, v)?;
                }
                Instr::Cmp { op, dst, lhs, rhs } => {
                    let a = th.stack[base + lhs as usize];
                    let b = th.stack[base + rhs as usize];
                    let r = match op {
                        CmpOp::Eq => values_equal(a, b),
                        CmpOp::Ne => !values_equal(a, b),
                        CmpOp::Lt => self.less_than(th, a, b, false)?,
                        CmpOp::Le => self.less_than(th, a, b, true)?,
                    };
                    th.stack[base + dst as usize] = Value::Bool(r);
                }
                Instr::Concat { dst, base: b, n } => {
                    *fuel -= n as i64;
                    let mut out: Vec<u8> = Vec::new();
                    for i in 0..n as usize {
                        let v = th.stack[base + b as usize + i];
                        match v {
                            Value::Str(s) => out.extend_from_slice(self.strings.get(s)),
                            Value::Int(_) | Value::Float(_) => {
                                out.extend_from_slice(fmt_number(v).as_bytes())
                            }
                            other => {
                                return Err(self.rt_err(
                                    th,
                                    format!(
                                        "attempt to concatenate a {} value",
                                        other.type_name()
                                    ),
                                ))
                            }
                        }
                    }
                    th.stack[base + dst as usize] = self.new_string(&out);
                }
                Instr::Jump { off } => {
                    jump(th, off);
                }
                Instr::Test { src, if_true, off } => {
                    if th.stack[base + src as usize].truthy() == if_true {
                        jump(th, off);
                    }
                }
                Instr::Call { base: b, nargs, nres } => {
                    let func_abs = base + b as usize;
                    let argc = if nargs == 0 {
                        th.top.saturating_sub(func_abs + 1)
                    } else {
                        (nargs - 1) as usize
                    };
                    self.do_call(th, func_abs, argc, nres, fuel)?;
                }
                Instr::Return { base: b, n } => {
                    let frame = th.frames.pop().unwrap();
                    self.close_upvals(th, frame.base);
                    let start = frame.base + b as usize;
                    let count = if n == 0 {
                        th.top.saturating_sub(start)
                    } else {
                        (n - 1) as usize
                    };
                    if th.frames.is_empty() {
                        let vals = th.stack[start..start + count].to_vec();
                        th.stack.clear();
                        th.top = 0;
                        return Ok(Some(vals));
                    }
                    let ret_to = frame.ret_to;
                    if frame.nres == 0 {
                        ensure_len(&mut th.stack, ret_to + count);
                        th.stack.copy_within(start..start + count, ret_to);
                        th.top = ret_to + count;
                    } else {
                        let want = (frame.nres - 1) as usize;
                        ensure_len(&mut th.stack, ret_to + want);
                        for i in 0..want {
                            th.stack[ret_to + i] = if i < count {
                                th.stack[start + i]
                            } else {
                                Value::Nil
                            };
                        }
                    }
                }
                Instr::Vararg { dst, n } => {
                    let varargs = std::mem::take(&mut th.frames.last_mut().unwrap().varargs);
                    let first = base + dst as usize;
                    if n == 0 {
                        ensure_len(&mut th.stack, first + varargs.len());
                        th.stack[first..first + varargs.len()].copy_from_slice(&varargs);
                        th.top = first + varargs.len();
                    } else {
                        let want = (n - 1) as usize;
                        ensure_len(&mut th.stack, first + want);
                        for i in 0..want {
                            th.stack[first + i] = varargs.get(i).copied().unwrap_or(Value::Nil);
                        }
                    }
                    th.frames.last_mut().unwrap().varargs = varargs;
                }
                Instr::Closure { dst, p } => {
                    *fuel -= 2;
                    let proto = th.frames.last().unwrap().proto.protos[p as usize].clone();
                    let mut ups = Vec::with_capacity(proto.upvals.len());
                    for d in &proto.upvals {
                        match *d {
                            UpvalDesc::Local(r) => {
                                let abs = base + r as usize;
                                ups.push(self.find_or_create_open(tid, th, abs));
                            }
                            UpvalDesc::Upval(i) => {
                                let cur = th.frames.last().unwrap().closure;
                                ups.push(self.closures[cur.0 as usize].upvals[i as usize]);
                            }
                        }
                    }
                    let cid = ClosId(self.closures.len() as u32);
                    self.closures.push(LuaClosure { proto, upvals: ups });
                    th.stack[base + dst as usize] = Value::Closure(cid);
                }
                Instr::Close { from } => {
                    self.close_upvals(th, base + from as usize);
                }
                Instr::ForPrep { base: b, off } => {
                    self.for_prep(th, base + b as usize, off)?;
                }
                Instr::ForLoop { base: b, off } => {
                    let a = base + b as usize;
                    match (th.stack[a], th.stack[a + 1], th.stack[a + 2]) {
                        (Value::Int(i), Value::Int(l), Value::Int(s)) => {
                            if let Some(ni) = i.checked_add(s)
                                && ((s > 0 && ni <= l) || (s < 0 && ni >= l)) {
                                    th.stack[a] = Value::Int(ni);
                                    th.stack[a + 3] = Value::Int(ni);
                                    jump(th, off);
                                }
                        }
                        (Value::Float(i), Value::Float(l), Value::Float(s)) => {
                            let ni = i + s;
                            if (s > 0.0 && ni <= l) || (s < 0.0 && ni >= l) {
                                th.stack[a] = Value::Float(ni);
                                th.stack[a + 3] = Value::Float(ni);
                                jump(th, off);
                            }
                        }
                        _ => unreachable!("ForLoop after ForPrep normalization"),
                    }
                }
                Instr::TForLoop { base: b, off } => {
                    let a = base + b as usize;
                    let v = th.stack[a + 3];
                    if v != Value::Nil {
                        th.stack[a + 2] = v;
                        jump(th, off);
                    }
                }
            }
        }
    }

    fn do_call(
        &mut self,
        th: &mut Thread,
        func_abs: usize,
        argc: usize,
        nres: u8,
        fuel: &mut i64,
    ) -> Result<(), RuntimeError> {
        *fuel -= 2;
        match th.stack[func_abs] {
            Value::Closure(cid) => {
                if th.frames.len() >= MAX_CALL_DEPTH {
                    return Err(self.rt_err(th, "stack overflow (too many nested calls)".into()));
                }
                let proto = self.closures[cid.0 as usize].proto.clone();
                let new_base = func_abs + 1;
                let np = proto.nparams as usize;
                let mut varargs = Vec::new();
                if proto.is_vararg && argc > np {
                    varargs.extend_from_slice(&th.stack[new_base + np..new_base + argc]);
                }
                ensure_len(&mut th.stack, new_base + proto.max_regs as usize);
                for i in argc..np {
                    th.stack[new_base + i] = Value::Nil;
                }
                th.frames.push(Frame {
                    closure: cid,
                    proto,
                    pc: 0,
                    base: new_base,
                    ret_to: func_abs,
                    nres,
                    varargs,
                });
            }
            Value::Native(nid) => {
                let args = th.stack[func_abs + 1..func_abs + 1 + argc].to_vec();
                let f = self.natives[nid.0 as usize].f;
                let res = f(self, &args).map_err(|message| self.rt_err(th, message))?;
                if nres == 0 {
                    ensure_len(&mut th.stack, func_abs + res.len());
                    th.stack[func_abs..func_abs + res.len()].copy_from_slice(&res);
                    th.top = func_abs + res.len();
                } else {
                    let want = (nres - 1) as usize;
                    ensure_len(&mut th.stack, func_abs + want);
                    for i in 0..want {
                        th.stack[func_abs + i] = res.get(i).copied().unwrap_or(Value::Nil);
                    }
                }
            }
            other => {
                return Err(self.rt_err(
                    th,
                    format!("attempt to call a {} value", other.type_name()),
                ))
            }
        }
        Ok(())
    }

    fn for_prep(&mut self, th: &mut Thread, a: usize, off: i32) -> Result<(), RuntimeError> {
        let init = th.stack[a];
        let limit = th.stack[a + 1];
        let step = th.stack[a + 2];
        let num = |v: Value, what: &str| -> Result<Value, RuntimeError> {
            match v {
                Value::Int(_) | Value::Float(_) => Ok(v),
                _ => Err(self.rt_err(th, format!("'for' {what} must be a number"))),
            }
        };
        let init = num(init, "initial value")?;
        let limit = num(limit, "limit")?;
        let step = num(step, "step")?;
        match (init, step) {
            (Value::Int(i0), Value::Int(s)) => {
                if s == 0 {
                    return Err(self.rt_err(th, "'for' step is zero".into()));
                }
                let l = match limit {
                    Value::Int(l) => Some(l),
                    Value::Float(f) => for_int_limit(f, s > 0),
                    _ => unreachable!(),
                };
                match l {
                    Some(l) if (s > 0 && i0 <= l) || (s < 0 && i0 >= l) => {
                        th.stack[a + 1] = Value::Int(l);
                        th.stack[a + 3] = Value::Int(i0);
                    }
                    _ => jump(th, off),
                }
            }
            _ => {
                // float loop
                let to_f = |v: Value| match v {
                    Value::Int(i) => i as f64,
                    Value::Float(f) => f,
                    _ => unreachable!(),
                };
                let (i0, l, s) = (to_f(init), to_f(limit), to_f(step));
                if s == 0.0 {
                    return Err(self.rt_err(th, "'for' step is zero".into()));
                }
                if (s > 0.0 && i0 <= l) || (s < 0.0 && i0 >= l) {
                    th.stack[a] = Value::Float(i0);
                    th.stack[a + 1] = Value::Float(l);
                    th.stack[a + 2] = Value::Float(s);
                    th.stack[a + 3] = Value::Float(i0);
                } else {
                    jump(th, off);
                }
            }
        }
        Ok(())
    }

    fn unary(&mut self, th: &Thread, op: UnaryOp, v: Value) -> Result<Value, RuntimeError> {
        match op {
            UnaryOp::Not => Ok(Value::Bool(!v.truthy())),
            UnaryOp::Neg => match v {
                Value::Int(i) => Ok(Value::Int(i.wrapping_neg())),
                Value::Float(f) => Ok(Value::Float(-f)),
                _ => Err(self.rt_err(
                    th,
                    format!("attempt to perform arithmetic on a {} value", v.type_name()),
                )),
            },
            UnaryOp::BNot => match to_int(v) {
                Some(i) => Ok(Value::Int(!i)),
                None => Err(self.rt_err(
                    th,
                    format!("attempt to perform bitwise operation on a {} value", v.type_name()),
                )),
            },
            UnaryOp::Len => match v {
                Value::Str(s) => Ok(Value::Int(self.strings.get(s).len() as i64)),
                Value::Table(t) => Ok(Value::Int(self.tables[t.0 as usize].length())),
                _ => Err(self.rt_err(
                    th,
                    format!("attempt to get length of a {} value", v.type_name()),
                )),
            },
        }
    }

    fn index_value(&self, th: &Thread, o: Value, k: Value) -> Result<Value, RuntimeError> {
        match o {
            Value::Table(t) => Ok(self.tables[t.0 as usize].get(k)),
            _ => Err(self.rt_err(th, format!("attempt to index a {} value", o.type_name()))),
        }
    }

    fn setindex_value(
        &mut self,
        th: &Thread,
        o: Value,
        k: Value,
        v: Value,
    ) -> Result<(), RuntimeError> {
        match o {
            Value::Table(t) => self.tables[t.0 as usize]
                .set(k, v)
                .map_err(|m| self.rt_err(th, m.into())),
            _ => Err(self.rt_err(th, format!("attempt to index a {} value", o.type_name()))),
        }
    }

    fn less_than(
        &self,
        th: &Thread,
        a: Value,
        b: Value,
        or_equal: bool,
    ) -> Result<bool, RuntimeError> {
        match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(if or_equal { x <= y } else { x < y }),
            (Value::Float(x), Value::Float(y)) => Ok(if or_equal { x <= y } else { x < y }),
            (Value::Int(x), Value::Float(y)) => Ok(if or_equal {
                int_le_float(x, y)
            } else {
                int_lt_float(x, y)
            }),
            (Value::Float(x), Value::Int(y)) => {
                if x.is_nan() {
                    return Ok(false);
                }
                // x < y  <=>  not (y <= x);  x <= y  <=>  not (y < x)
                Ok(if or_equal { !int_lt_float(y, x) } else { !int_le_float(y, x) })
            }
            (Value::Str(x), Value::Str(y)) => {
                let (xs, ys) = (self.strings.get(x), self.strings.get(y));
                Ok(if or_equal { xs <= ys } else { xs < ys })
            }
            _ => Err(self.rt_err(
                th,
                format!("attempt to compare {} with {}", a.type_name(), b.type_name()),
            )),
        }
    }

    fn frame_upval(&self, th: &Thread, up: u8) -> UpvalId {
        let cur = th.frames.last().unwrap().closure;
        self.closures[cur.0 as usize].upvals[up as usize]
    }

    fn read_upval(&self, tid: ThreadId, th: &Thread, id: UpvalId) -> Value {
        match self.upvals[id.0 as usize] {
            Upval::Closed(v) => v,
            Upval::Open(t, idx) => {
                if t == tid {
                    th.stack[idx]
                } else {
                    self.threads[t.0 as usize].stack[idx]
                }
            }
        }
    }

    fn write_upval(&mut self, tid: ThreadId, th: &mut Thread, id: UpvalId, v: Value) {
        match self.upvals[id.0 as usize] {
            Upval::Closed(_) => self.upvals[id.0 as usize] = Upval::Closed(v),
            Upval::Open(t, idx) => {
                if t == tid {
                    th.stack[idx] = v;
                } else {
                    self.threads[t.0 as usize].stack[idx] = v;
                }
            }
        }
    }

    fn find_or_create_open(&mut self, tid: ThreadId, th: &mut Thread, abs: usize) -> UpvalId {
        match th.open_upvals.binary_search_by_key(&abs, |&(i, _)| i) {
            Ok(i) => th.open_upvals[i].1,
            Err(pos) => {
                let id = self.new_upval(Upval::Open(tid, abs));
                th.open_upvals.insert(pos, (abs, id));
                id
            }
        }
    }

    fn close_upvals(&mut self, th: &mut Thread, from_abs: usize) {
        while let Some(&(idx, id)) = th.open_upvals.last() {
            if idx < from_abs {
                break;
            }
            self.upvals[id.0 as usize] = Upval::Closed(th.stack[idx]);
            th.open_upvals.pop();
        }
    }

    fn rt_err(&self, th: &Thread, message: String) -> RuntimeError {
        RuntimeError { message, line: line_of(th) }
    }
}

impl Execution {
    /// Runs the script for at most `fuel` units of work (roughly one unit
    /// per VM instruction, with surcharges for calls and allocations).
    /// Returns `Step::Pending` if the budget ran out — call again to resume.
    pub fn step(&mut self, lua: &mut Lua, fuel: u64) -> Result<Step, Error> {
        if self.finished {
            return Err(Error::Runtime(RuntimeError {
                message: "execution already finished".into(),
                line: 0,
            }));
        }
        let budget = fuel.min(i64::MAX as u64) as i64;
        let mut remaining = budget - self.debt;
        if remaining <= 0 {
            self.debt -= budget;
            return Ok(Step::Pending);
        }
        match lua.run(self.thread, &mut remaining) {
            Ok(Some(vals)) => {
                self.finished = true;
                Ok(Step::Done(vals))
            }
            Ok(None) => {
                self.debt = (-remaining).max(0);
                Ok(Step::Pending)
            }
            Err(e) => {
                self.finished = true;
                Err(Error::Runtime(e))
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
}

// ---- free helpers ----

fn kval(th: &Thread, k: u16) -> Value {
    th.frames.last().unwrap().proto.consts[k as usize]
}

fn jump(th: &mut Thread, off: i32) {
    let f = th.frames.last_mut().unwrap();
    f.pc = (f.pc as i64 + off as i64) as usize;
}

fn line_of(th: &Thread) -> u32 {
    let f = th.frames.last().unwrap();
    f.proto.lines.get(f.pc.wrapping_sub(1)).copied().unwrap_or(0)
}

fn ensure_len(stack: &mut Vec<Value>, len: usize) {
    if stack.len() < len {
        stack.resize(len, Value::Nil);
    }
}

/// Lua `==` (without metamethods): raw equality plus cross int/float.
pub(crate) fn values_equal(a: Value, b: Value) -> bool {
    match (a, b) {
        (Value::Int(x), Value::Float(y)) | (Value::Float(y), Value::Int(x)) => {
            float_to_exact_int(y) == Some(x)
        }
        _ => a == b,
    }
}

/// Converts to an integer for bitwise ops (floats with exact integral value).
fn to_int(v: Value) -> Option<i64> {
    match v {
        Value::Int(i) => Some(i),
        Value::Float(f) => float_to_exact_int(f),
        _ => None,
    }
}

fn to_float(v: Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(i as f64),
        Value::Float(f) => Some(f),
        _ => None,
    }
}

/// `i < f` with exact semantics across the full i64/f64 ranges.
fn int_lt_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= 9.223372036854776e18 {
        return true; // f >= 2^63 > any i64
    }
    if f < -9.223372036854776e18 {
        return false;
    }
    let ff = f.floor();
    let fi = ff as i64;
    i < fi || (i == fi && f > ff)
}

fn int_le_float(i: i64, f: f64) -> bool {
    if f.is_nan() {
        return false;
    }
    if f >= 9.223372036854776e18 {
        return true;
    }
    if f < -9.223372036854776e18 {
        return false;
    }
    let ff = f.floor();
    let fi = ff as i64;
    i < fi || (i == fi && f >= ff)
}

/// Converts a float limit of an integer `for` loop per Lua 5.4 (floor/ceil
/// toward the loop interior, clamped). `None` means the loop is empty.
fn for_int_limit(f: f64, step_positive: bool) -> Option<i64> {
    if f.is_nan() {
        return None;
    }
    if step_positive {
        if f < -9.223372036854776e18 {
            None
        } else if f >= 9.223372036854776e18 {
            Some(i64::MAX)
        } else {
            Some(f.floor() as i64)
        }
    } else if f >= 9.223372036854776e18 {
        None
    } else if f < -9.223372036854776e18 {
        Some(i64::MIN)
    } else {
        Some(f.ceil() as i64)
    }
}

fn arith(op: ArithOp, a: Value, b: Value) -> Result<Value, String> {
    use ArithOp::*;
    let num_err = |v: Value| {
        format!("attempt to perform arithmetic on a {} value", v.type_name())
    };
    let int_err = |v: Value| match v {
        Value::Float(_) => "number has no integer representation".to_string(),
        _ => format!("attempt to perform bitwise operation on a {} value", v.type_name()),
    };
    match op {
        Add | Sub | Mul => match (a, b) {
            (Value::Int(x), Value::Int(y)) => Ok(Value::Int(match op {
                Add => x.wrapping_add(y),
                Sub => x.wrapping_sub(y),
                Mul => x.wrapping_mul(y),
                _ => unreachable!(),
            })),
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                Ok(Value::Float(match op {
                    Add => x + y,
                    Sub => x - y,
                    Mul => x * y,
                    _ => unreachable!(),
                }))
            }
        },
        Div => {
            let x = to_float(a).ok_or_else(|| num_err(a))?;
            let y = to_float(b).ok_or_else(|| num_err(b))?;
            Ok(Value::Float(x / y))
        }
        Pow => {
            let x = to_float(a).ok_or_else(|| num_err(a))?;
            let y = to_float(b).ok_or_else(|| num_err(b))?;
            Ok(Value::Float(x.powf(y)))
        }
        IDiv => match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if y == 0 {
                    return Err("attempt to perform 'n//0'".into());
                }
                let q = x.wrapping_div(y);
                let q = if x.wrapping_rem(y) != 0 && (x < 0) != (y < 0) {
                    q - 1
                } else {
                    q
                };
                Ok(Value::Int(q))
            }
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                Ok(Value::Float((x / y).floor()))
            }
        },
        Mod => match (a, b) {
            (Value::Int(x), Value::Int(y)) => {
                if y == 0 {
                    return Err("attempt to perform 'n%0'".into());
                }
                let r = x.wrapping_rem(y);
                Ok(Value::Int(if r != 0 && (r < 0) != (y < 0) { r + y } else { r }))
            }
            _ => {
                let x = to_float(a).ok_or_else(|| num_err(a))?;
                let y = to_float(b).ok_or_else(|| num_err(b))?;
                let r = x % y;
                Ok(Value::Float(if r != 0.0 && (r < 0.0) != (y < 0.0) {
                    r + y
                } else {
                    r
                }))
            }
        },
        BAnd | BOr | BXor => {
            let x = to_int(a).ok_or_else(|| int_err(a))?;
            let y = to_int(b).ok_or_else(|| int_err(b))?;
            Ok(Value::Int(match op {
                BAnd => x & y,
                BOr => x | y,
                BXor => x ^ y,
                _ => unreachable!(),
            }))
        }
        Shl | Shr => {
            let x = to_int(a).ok_or_else(|| int_err(a))?;
            let y = to_int(b).ok_or_else(|| int_err(b))?;
            // Lua shifts are logical; a negative count shifts the other way,
            // and counts >= 64 produce zero
            let n = if op == Shr { y.checked_neg().unwrap_or(i64::MAX) } else { y };
            Ok(Value::Int(shift_left_logical(x, n)))
        }
    }
}

fn shift_left_logical(x: i64, n: i64) -> i64 {
    if n <= -64 || n >= 64 {
        0
    } else if n >= 0 {
        ((x as u64) << n) as i64
    } else {
        ((x as u64) >> -n) as i64
    }
}
